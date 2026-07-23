#!/usr/bin/env python3
"""AB-parity compare site server: static site + multi-run data + deep-inspect API.

Data source: run directories under <repo>/runs/ (override: ABGEN_RUNS_DIR or
--runs DIR). Each run is a self-contained dataset:

  runs/<run-id>/
    site-data.json                 # (or data.json) built rows; served as /r/<id>/data.json
    renders/<pair>-{up,ours}-a<i>.png / -{up,ours}.gif / -{up,ours}.inventory.json
    tex-images/<pair>-{up,ours}.png
    upstream/<entity>/<platform>/<bundleFile>
    ours/<entity>/<platform>/<bundleFile>
    analysis/pairs.jsonl           # run-relative ours_path/upstream_path per pair

Endpoints (GET, JSON unless noted):
  /api/runs                      run list (id + stats) + lod dataset list, newest first
  /api/preflight[?content=&abcdn=]  setup wizard checks (see wizard.html)
  /api/row?run=&plat=&pair=      row from the run's data (+labels/thresholds)
  /api/locate?run=&plat=&pair=   resolve both sides' bundle files + inventories
  /api/bytediff?a=&b=            sizes, sha256, first-diff, differing ranges
  /api/xxd?f=&off=&len=          hex dump chunk (text), len<=4096
  /api/dump?tool=obj|tex|mat&f=  run objdump/texdump/matdump on a bundle (text)
  /api/inv?f=                    serve an inventory json by path token
  /r/<run-id>/<path>             static files from the run dir (evidence images)
  /r/<run-id>/data.json          run rows; ETag/Last-Modified + If-None-Match
                                 304s (the live index polls this every ~20s)

File paths are jailed to the runs root. Tools run with a timeout, output capped.
When no runs exist, / redirects to the setup wizard.

Anonymous-exposure hardening (the port may face the open internet directly):
per-IP sliding-window rate limits on the deep-inspect endpoints, a global
semaphore around subprocess/file-heavy work, SSRF-guarded preflight probes
(operator-configured URLs are trusted verbatim; caller-supplied ones must
resolve to public addresses), size-capped reads with explicit truncation
markers, and a sanitized preflight payload for non-local callers.
"""
import json, os, re, sys, shutil, hashlib, subprocess, socket, ipaddress, threading, time, urllib.parse, urllib.request, urllib.error
from collections import deque
from concurrent.futures import ThreadPoolExecutor
from http.server import ThreadingHTTPServer, SimpleHTTPRequestHandler

SITE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(SITE)
sys.path.insert(0, os.path.join(REPO, 'pipeline'))
import harness_contract as hc  # noqa: E402
from abgencompare import toolconfig  # noqa: E402
RUNS = os.environ.get('ABGEN_RUNS_DIR') or os.path.join(REPO, 'runs')
if '--runs' in sys.argv:
    RUNS = sys.argv[sys.argv.index('--runs') + 1]
RUNS = os.path.realpath(RUNS)
ALLOWED_ROOTS = (RUNS,)

def _tool(name, env):
    p = os.environ.get(env)
    if p:
        return p
    tdir = os.environ.get('ABGEN_TOOLS_DIR')
    cands = ([os.path.join(tdir, name)] if tdir else []) + [
        os.path.join(REPO, 'target', 'release', 'examples', name),
        os.path.join(REPO, 'result', 'bin', name)]
    for cand in cands:
        if os.path.isfile(cand):
            return cand
    return os.path.join(REPO, 'target', 'release', 'examples', name)

TOOLS = {
    'obj': _tool('objdump', 'ABGEN_OBJDUMP'),
    'tex': _tool('texdump', 'ABGEN_TEXDUMP'),
    'mat': _tool('matdump', 'ABGEN_MATDUMP'),
}

RUN_ID = re.compile(r'[A-Za-z0-9][A-Za-z0-9._-]*$')

def run_dir(run):
    if not run or not RUN_ID.match(run):
        return None
    d = os.path.join(RUNS, run)
    return d if os.path.isdir(d) else None

def run_data_path(run):
    d = run_dir(run)
    if not d:
        return None
    for cand in ('site-data.json', 'data.json'):
        p = os.path.join(d, cand)
        if os.path.isfile(p):
            return p
    return None

_cache = {}
def cached_json(path):
    try:
        mt = os.path.getmtime(path)
    except OSError:
        return None
    ent = _cache.get(path)
    if ent and ent[0] == mt:
        return ent[1]
    with open(path) as f:
        val = json.load(f)
    _cache[path] = (mt, val)
    return val

def list_runs():
    out = []
    if not os.path.isdir(RUNS):
        return out
    for name in sorted(os.listdir(RUNS)):
        p = run_data_path(name)
        if not p:
            continue
        d = cached_json(p) or {}
        st = d.get('stats') or {}
        out.append({
            'id': name,
            'generated': d.get('generated'),
            'demo': bool(d.get('demo')),
            'description': d.get('description'),
            'labels': d.get('labels'),
            'pairs': st.get('pairs'), 'entities': st.get('entities'),
            'platforms': st.get('platforms') or {},
            'kinds': st.get('kinds') or {}, 'tags': st.get('tags') or {},
            'verdicts': st.get('labels') or {},
        })
    out.sort(key=lambda r: r.get('generated') or '', reverse=True)
    return out

def list_lod_runs():
    """LOD parity datasets (lod-data.json runs), newest first, with a `match`
    aggregate using lod.html's badge rule: has prod, tri ratio in 0.6–1.05
    (or unknown), zero failed structural checks."""
    out = []
    if not os.path.isdir(RUNS):
        return out
    for name in sorted(os.listdir(RUNS)):
        d = run_dir(name)
        if not d:
            continue
        p = os.path.join(d, 'lod-data.json')
        if not os.path.isfile(p):
            continue
        data = cached_json(p) or {}
        scenes = data.get('scenes') or []
        st = data.get('stats') or {}
        match = 0
        for s in scenes:
            prod = s.get('prod')
            if not prod:
                continue
            ours = s.get('ours') or {}
            ratio = (ours['tris'] / prod['tris']
                     if prod.get('tris') and ours.get('tris') else None)
            if (ratio is None or 0.6 <= ratio <= 1.05) and not s.get('fails'):
                match += 1
        out.append({
            'id': name, 'generated': data.get('generated'),
            'platform': data.get('platform'),
            'scenes': st.get('scenes', len(scenes)),
            'with_prod': st.get('with_prod'), 'ours_only': st.get('ours_only'),
            'match': match,
        })
    out.sort(key=lambda r: r.get('generated') or '', reverse=True)
    return out

_pairs_cache = {}
def pairs_index(run):
    """pair_id -> pairing row (run-relative ours_path/upstream_path) from
    analysis/pairs.jsonl. mtime-aware cache: the live-JIT rolling run rewrites
    pairs.jsonl continuously, so a plain lru_cache would go stale."""
    d = run_dir(run)
    if not d:
        return {}
    path = os.path.join(d, 'analysis', 'pairs.jsonl')
    try:
        mt = os.path.getmtime(path)
    except OSError:
        return {}
    ent = _pairs_cache.get(run)
    if ent and ent[0] == mt:
        return ent[1]
    idx = {}
    try:
        for line in open(path):
            line = line.strip()
            if not line:
                continue
            r = json.loads(line)
            pid = r.get('pair_id') or r.get('pair')
            if pid:
                idx[pid] = r
    except (FileNotFoundError, ValueError):
        pass
    if len(_pairs_cache) > 16:
        _pairs_cache.clear()
    _pairs_cache[run] = (mt, idx)
    return idx

def safe(path):
    if not path or not isinstance(path, str):
        return None
    rp = os.path.realpath(path)
    return rp if any(rp == r or rp.startswith(r + os.sep) for r in ALLOWED_ROOTS) \
        and os.path.isfile(rp) else None

def run_rel(run, path):
    """Resolve a (run-relative or absolute) pairing path inside the jail."""
    if not path or not isinstance(path, str):
        return None
    if not os.path.isabs(path):
        d = run_dir(run)
        if not d:
            return None
        path = os.path.join(d, path)
    return safe(path)

def find_row(run, plat, pair):
    p = run_data_path(run)
    d = cached_json(p) if p else None
    if not d:
        return None, None
    for ent in d.get('entities', []):
        for r in ent['rows']:
            if r['pair'] == pair and (not plat or r.get('platform') == plat):
                return ent['entity'], r
    return None, None

def locate(run, plat, pair):
    """Canonical run layout only: bundles from analysis/pairs.jsonl (run-relative
    paths), inventories at renders/<pair>-{up,ours}.inventory.json."""
    entity, row = find_row(run, plat, pair)
    out = {'entity': entity, 'row': row, 'up': {}, 'ab': {}}
    pr = pairs_index(run).get(pair) or {}
    out['ab']['bundle'] = run_rel(run, pr.get('ours_path'))
    out['up']['bundle'] = next(
        (b for b in (run_rel(run, pr.get(k))
                     for k in ('upstream_path', 'upstream_local_path', 'upstream_file'))
         if b), None)
    d = run_dir(run)
    if d:
        out['up']['inv'] = safe(os.path.join(d, 'renders', f'{pair}-up.inventory.json'))
        out['ab']['inv'] = safe(os.path.join(d, 'renders', f'{pair}-ours.inventory.json'))
    return out

def _env_int(name, default):
    try:
        return int(os.environ[name])
    except (KeyError, ValueError):
        return default

MAX_DIFF_BYTES = _env_int('ABGEN_MAX_DIFF_BYTES', 8 << 20)
MAX_DUMP_INPUT = _env_int('ABGEN_MAX_DUMP_BYTES', 64 << 20)
MAX_TEXT_OUT = _env_int('ABGEN_MAX_TEXT_OUT', 400000)

def bytediff(a, b):
    """Compare at most MAX_DIFF_BYTES per side (full-file reads on anonymous
    input invite memory DoS). Oversize files are diffed on the capped prefix
    and flagged truncated — sha256/pctDiff then describe the prefix only."""
    sa, sb = os.path.getsize(a), os.path.getsize(b)
    truncated = sa > MAX_DIFF_BYTES or sb > MAX_DIFF_BYTES
    with open(a, 'rb') as fa, open(b, 'rb') as fb:
        ra, rb = fa.read(MAX_DIFF_BYTES), fb.read(MAX_DIFF_BYTES)
    n = min(len(ra), len(rb))
    first = -1
    diffs = 0
    ranges = []
    in_range = False
    start = 0
    for i in range(n):
        if ra[i] != rb[i]:
            diffs += 1
            if first < 0:
                first = i
            if not in_range:
                in_range, start = True, i
        elif in_range:
            in_range = False
            if len(ranges) < 32:
                ranges.append([start, i - 1])
    if in_range and len(ranges) < 32:
        ranges.append([start, n - 1])
    diffs += abs(len(ra) - len(rb))
    out = {
        'sizeA': sa, 'sizeB': sb,
        'sha256A': hashlib.sha256(ra).hexdigest(), 'sha256B': hashlib.sha256(rb).hexdigest(),
        'identical': not truncated and ra == rb, 'firstDiff': first,
        'differingBytes': diffs, 'pctDiff': round(100 * diffs / max(1, max(len(ra), len(rb))), 3),
        'ranges': ranges,
    }
    if truncated:
        out['truncated'] = True
        out['limit'] = MAX_DIFF_BYTES
        out['note'] = f'compared first {MAX_DIFF_BYTES} bytes of each side only'
    return out

def xxd(path, off, ln):
    with open(path, 'rb') as f:
        f.seek(off)
        data = f.read(ln)
    lines = []
    for i in range(0, len(data), 16):
        chunk = data[i:i+16]
        hexs = ' '.join(f'{b:02x}' for b in chunk)
        asc = ''.join(chr(b) if 32 <= b < 127 else '.' for b in chunk)
        lines.append(f'{off+i:08x}: {hexs:<47} {asc}')
    return '\n'.join(lines)

DEFAULT_CONTENT = os.environ.get('ABGEN_CATALYST_URL', 'https://peer.decentraland.org/content')
DEFAULT_ABCDN = os.environ.get('ABGEN_AB_CDN', 'https://ab-cdn.decentraland.org')

SHADER_SHA = '5a5ce6694c85b77be165e367fc510f2c8f06a05fa1422330fcff4c3793d6c4b5'
TEMPLATE_SHAS = {
    'all-types.windows.bundle': '7a2f876ce9436a4ee7fb66c2c4b206dc2f844140f081efee231cfaab2ab6db67',
    'animated-types.windows.bundle': '91236453b18b4badd5f5d66412b83d8164f46c03ab577b94b1ff857de9d2e62f',
    'emote-types.windows.bundle': 'f0f0246cb218cbb31185f66f71d75ed3370aca85dc3af6582de7aba78e02c1f4',
    'skinned-types.windows.bundle': 'b2ce6065b03ddb9e62d1f8c2e5a1ec7e20d0d92faf4beb736156901d82b5e6d3',
}

def _sha256(path):
    h = hashlib.sha256()
    with open(path, 'rb') as f:
        for chunk in iter(lambda: f.read(1 << 20), b''):
            h.update(chunk)
    return h.hexdigest()

def _assert_public_url(url):
    parts = urllib.parse.urlsplit(url)
    if parts.scheme not in ('http', 'https'):
        raise ValueError(f'scheme {parts.scheme!r} not allowed')
    host = parts.hostname
    if not host:
        raise ValueError('no host in URL')
    port = parts.port or (443 if parts.scheme == 'https' else 80)
    cgnat = ipaddress.ip_network('100.64.0.0/10')
    for info in socket.getaddrinfo(host, port, proto=socket.IPPROTO_TCP):
        ip = ipaddress.ip_address(info[4][0])
        if (ip.is_private or ip.is_loopback or ip.is_link_local or ip.is_reserved
                or ip.is_multicast or ip.is_unspecified
                or (ip.version == 4 and ip in cgnat)):
            raise ValueError(f'host {host} resolves to non-public address {ip}')


def _http_ok(url, timeout=6, trusted=False):
    if not trusted:
        _assert_public_url(url)
    req = urllib.request.Request(url, headers={'User-Agent': 'abgen-preflight'})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.status, r.read(512)

def ck(cid, name, status, detail, hint=None, optional=False):
    c = {'id': cid, 'name': name, 'status': status, 'detail': detail, 'optional': optional}
    if hint:
        c['hint'] = hint
    return c

def check_binary():
    for cand in (os.path.join(REPO, 'target', 'release', 'abgen'),
                 os.path.join(REPO, 'result', 'bin', 'abgen')):
        if os.path.isfile(cand):
            banner = ''
            probe = cand.replace('abgen', 'abgen-build') if os.path.isfile(
                cand.replace('abgen', 'abgen-build')) else None
            if probe:
                try:
                    r = subprocess.run([probe], capture_output=True, text=True, timeout=15)
                    banner = (r.stderr or r.stdout or '').splitlines()[0][:100]
                except Exception as e:
                    return ck('binary', 'abgen binaries built', 'warn',
                              f'{cand} exists but abgen-build probe failed: {e}',
                              'rebuild: cargo build --release   (or: nix build .#)')
            mt = os.path.getmtime(cand)
            import datetime
            when = datetime.datetime.fromtimestamp(mt).strftime('%Y-%m-%d %H:%M')
            return ck('binary', 'abgen binaries built', 'ok',
                      f'{cand} ({os.path.getsize(cand)//1024//1024} MB, built {when})'
                      + (f' · banner: "{banner}"' if banner else ''))
    return ck('binary', 'abgen binaries built', 'fail',
              'no target/release/abgen or result/bin/abgen',
              'cargo build --release   (see README Toolchain: needs cc/c++, cmake, make) '
              '— or: nix build .#')

def check_tools():
    missing = [f'{k}:{v}' for k, v in TOOLS.items() if not os.path.isfile(v)]
    if not missing:
        return ck('tools', 'bundle dump tools (objdump/texdump/matdump)', 'ok',
                  os.path.dirname(TOOLS['obj']))
    return ck('tools', 'bundle dump tools (objdump/texdump/matdump)', 'warn',
              'missing: ' + ', '.join(missing),
              'cargo build --release --examples — without them the detail page\'s '
              'structure/textures/materials tabs degrade to "no bytes" (site still works)',
              optional=True)

def check_runtime_data():
    probs = []
    sh = os.path.join(REPO, 'crate', 'shader', 'scene_ignore_windows')
    if not os.path.isfile(sh):
        probs.append('shader bundle missing (crate/shader/scene_ignore_windows)')
    elif _sha256(sh) != SHADER_SHA:
        probs.append('shader bundle sha256 drifted')
    for name, want in TEMPLATE_SHAS.items():
        p = os.path.join(REPO, 'template', name)
        if not os.path.isfile(p):
            probs.append(f'template/{name} missing')
        elif _sha256(p) != want:
            probs.append(f'template/{name} sha256 drifted')
    if not probs:
        return ck('runtime-data', 'runtime data bootstrapped (templates + shader)', 'ok',
                  '4 typetree-donor templates + scene_ignore shader verified against pins')
    return ck('runtime-data', 'runtime data bootstrapped (templates + shader)', 'fail',
              '; '.join(probs),
              'restore from git history (git checkout -- crate/shader/ template/) '
              'and re-run scripts/bootstrap-runtime.sh')

def check_content(url, trusted=False):
    probe = url.rstrip('/') + '/status'
    try:
        st, body = _http_ok(probe, trusted=trusted)
        return ck('content-server', 'content server reachable', 'ok',
                  f'GET {probe} → {st}')
    except urllib.error.HTTPError as e:
        return ck('content-server', 'content server reachable', 'warn',
                  f'{probe} → HTTP {e.code} (host reachable, unexpected status)',
                  'expected a catalyst content-server root (…/content) exposing /status; '
                  'check the URL path')
    except Exception as e:
        return ck('content-server', 'content server reachable', 'fail',
                  f'{probe} → {type(e).__name__}: {e}',
                  'set the content server URL above (env ABGEN_CATALYST_URL); any '
                  'catalyst content endpoint works, e.g. https://peer.decentraland.org/content')

ABCDN_PROBE = '/manifest/QmPAyzWU7gtdVRr9DGohiRzrSXL67NQdRuMwpfecoireUD_mac.json'

def check_abcdn(url, trusted=False):
    probe = url.rstrip('/') + ABCDN_PROBE
    try:
        st, _ = _http_ok(probe, trusted=trusted)
        return ck('ab-cdn', 'ab-cdn reachable', 'ok', f'GET {probe} → {st}')
    except urllib.error.HTTPError as e:
        return ck('ab-cdn', 'ab-cdn reachable', 'warn',
                  f'{probe} → HTTP {e.code} (host reachable, probe manifest missing)',
                  'the host answered but the probe manifest 404d — fine if this is a '
                  'partial mirror; fetch will flag per-entity gaps')
    except Exception as e:
        return ck('ab-cdn', 'ab-cdn reachable', 'fail',
                  f'{probe} → {type(e).__name__}: {e}',
                  'set the ab-cdn base URL above (env ABGEN_AB_CDN); upstream is '
                  'https://ab-cdn.decentraland.org')

def check_disk():
    path = RUNS if os.path.isdir(RUNS) else REPO
    du = shutil.disk_usage(path)
    free_gb = du.free / 1e9
    detail = f'{free_gb:.1f} GB free at {path}'
    if free_gb < 2:
        return ck('disk', 'disk space for runs', 'fail', detail,
                  'a render campaign needs bundles ×2 sides + ~1MB/shot; free ≥2 GB')
    if free_gb < 20:
        return ck('disk', 'disk space for runs', 'warn', detail,
                  'fine for small runs; large campaigns (300+ pairs × 2 sides × N shots) want 20+ GB')
    return ck('disk', 'disk space for runs', 'ok', detail)

UNITY_NOTE = ('required only for render verdicts; headless analysis '
              '(pairing, byte/structure diff, texture decode) works without')

MODE_NOTES = {
    'unity-explorer': 'client-faithful URP/shader environment (recommended)',
    'template': 'minimal template — comparative baseline, not client-exact',
    'custom': 'custom project — must be URP + linear color space',
}

def check_unity():
    """Editor and project are TWO separate inputs — checked separately."""
    out = []
    vals, srcs = toolconfig.effective()
    ub, up = vals['unity_editor'], vals['unity_project']
    if not ub:
        out.append(ck('unity-editor', 'Unity editor (OPTIONAL)', 'skip',
                      f'not configured — {UNITY_NOTE}',
                      './pipeline/abgen-compare config set unity_editor /path/to/Unity '
                      '(or export ABGEN_UNITY_BINARY) to enable the render harness',
                      optional=True))
    elif not os.path.isfile(ub):
        out.append(ck('unity-editor', 'Unity editor (OPTIONAL)', 'fail',
                      f'unity_editor={ub} ({srcs["unity_editor"]}) does not exist',
                      None, optional=True))
    else:
        try:
            r = subprocess.run([ub, '-batchmode', '-version'],
                               capture_output=True, text=True, timeout=60)
            v = (r.stdout or r.stderr or '').strip().splitlines()
            v = v[0] if v else ''
            if r.returncode == 0 and v:
                out.append(ck('unity-editor', 'Unity editor (OPTIONAL)', 'ok',
                              f'{ub} ({srcs["unity_editor"]}) · batchmode -version → "{v}" '
                              f'(licensed probe passed)', None, optional=True))
            else:
                out.append(ck('unity-editor', 'Unity editor (OPTIONAL)', 'warn',
                              f'-batchmode -version exited {r.returncode}: {v[:120]}',
                              'a license/activation problem usually surfaces here; '
                              'run the editor GUI once to activate', optional=True))
        except Exception as e:
            out.append(ck('unity-editor', 'Unity editor (OPTIONAL)', 'warn',
                          f'version probe failed: {e}', None, optional=True))
    if not up:
        out.append(ck('unity-project', 'render-harness Unity project (OPTIONAL)', 'skip',
                      f'not configured — {UNITY_NOTE}',
                      './pipeline/abgen-compare config set unity_project '
                      '/path/to/unity-explorer/Explorer — a decentraland/unity-explorer '
                      'checkout is the client-faithful host (see harness/README.md; '
                      'private UPM deps may need a vendored fork). Self-contained '
                      'fallback: cp -r harness/project-template /path/to/ab-harness-project',
                      optional=True))
    elif not os.path.isdir(os.path.join(up, 'Assets')):
        out.append(ck('unity-project', 'render-harness Unity project (OPTIONAL)', 'fail',
                      f'{up} ({srcs["unity_project"]}) has no Assets/ dir',
                      'point unity_project at the project ROOT (the dir holding Assets/); '
                      'for unity-explorer that is the Explorer/ subdir', optional=True))
    else:
        mode = hc.project_mode(up)
        out.append(ck('unity-project', 'render-harness Unity project (OPTIONAL)', 'ok',
                      f'{up} ({srcs["unity_project"]}) · mode: {mode} — '
                      + MODE_NOTES.get(mode, mode), None, optional=True))
        pv = hc.project_editor_version(up)
        ev = hc.editor_version_from_path(ub) if ub else None
        if pv and ev and pv != ev:
            out.append(ck('unity-version-match', 'editor ↔ project version (OPTIONAL)', 'warn',
                          f'editor {ev} vs ProjectVersion.txt {pv}',
                          'Unity migrates/reimports on version change (slow first run) '
                          'and renders may drift from campaign baselines — prefer the '
                          'same 6000.x version the project was stamped with',
                          optional=True))
        else:
            out.append(ck('unity-version-match', 'editor ↔ project version (OPTIONAL)', 'ok',
                          f'editor {ev or "unknown (no version token in path)"} · project '
                          f'{pv or "unstamped (template: Unity stamps it on first open)"}',
                          None, optional=True))
        try:
            st = hc.script_status(up)
            foreign = sorted(k for k, v in st.items() if v == 'foreign')
            pending = {k: v for k, v in st.items() if v != 'current'}
            if not pending:
                out.append(ck('unity-harness-scripts', 'harness scripts in project (OPTIONAL)',
                              'ok', 'all current in Assets/Editor/ (marker-managed)',
                              None, optional=True))
            else:
                out.append(ck('unity-harness-scripts', 'harness scripts in project (OPTIONAL)',
                              'warn' if foreign else 'ok',
                              ', '.join(f'{k}: {v}' for k, v in sorted(st.items())),
                              ('same-named file(s) WITHOUT the abgen marker: '
                               + ', '.join(foreign) + ' — the render stage will overwrite '
                               'them (with a warning); back up local edits first'
                               ) if foreign else
                              'auto-installed/refreshed at render time '
                              '(harness_contract.ensure_scripts) — nothing to do',
                              optional=True))
        except FileNotFoundError as e:
            out.append(ck('unity-harness-scripts', 'harness scripts in project (OPTIONAL)',
                          'fail', str(e), None, optional=True))
    return out

def check_python():
    missing = []
    for mod in ('numpy', 'PIL'):
        try:
            __import__(mod)
        except ImportError:
            missing.append(mod)
    if not missing:
        return ck('python-pixel', 'python3 + numpy + pillow (pixel classify)', 'ok',
                  f'{sys.version.split()[0]} at {sys.executable}; numpy+PIL importable')
    return ck('python-pixel', 'python3 + numpy + pillow (pixel classify)', 'warn',
              f'python3 ok ({sys.version.split()[0]}) but missing: ' + ', '.join(missing),
              'pip install numpy pillow — or on NixOS run the server inside: '
              'nix shell nixpkgs#python3 nixpkgs#python3Packages.numpy nixpkgs#python3Packages.pillow. '
              'The browser-side red-diff/ppm engine works regardless; numpy/PIL is for '
              'the batch classifier.')

def preflight(content_url, abcdn_url):
    with ThreadPoolExecutor(max_workers=8) as ex:
        f_bin = ex.submit(check_binary)
        f_tools = ex.submit(check_tools)
        f_rt = ex.submit(check_runtime_data)
        f_ct = ex.submit(check_content, content_url, content_url == DEFAULT_CONTENT)
        f_ab = ex.submit(check_abcdn, abcdn_url, abcdn_url == DEFAULT_ABCDN)
        f_dk = ex.submit(check_disk)
        f_un = ex.submit(check_unity)
        f_py = ex.submit(check_python)
        checks = [f_bin.result(), f_rt.result(), f_ct.result(), f_ab.result(),
                  f_dk.result(), *f_un.result(), f_py.result(), f_tools.result()]
    runs = list_runs()
    tc_vals, tc_srcs = toolconfig.effective()
    return {
        'config': {
            'repo': REPO, 'runs_dir': RUNS,
            'content_server': content_url, 'ab_cdn': abcdn_url,
            'unity_binary': tc_vals['unity_editor'],
            'unity_project': tc_vals['unity_project'],
            'win_staging': tc_vals['win_staging'],
            'tool_config_sources': tc_srcs,
            'tools': TOOLS,
        },
        'checks': checks,
        'runs': len(runs),
        'ok': all(c['status'] != 'fail' or c.get('optional') for c in checks),
    }

PUBLIC_CHECK_DETAIL = ('content-server', 'ab-cdn')

def sanitize_preflight(pf):
    """Anonymous view of the preflight payload: statuses + the two URLs only —
    no filesystem layout, no exception strings, no tool/config paths."""
    checks = []
    for c in pf['checks']:
        keep = c['id'] in PUBLIC_CHECK_DETAIL and c['status'] != 'fail'
        sc = {'id': c['id'], 'name': c['name'], 'status': c['status'],
              'detail': c['detail'] if keep else '',
              'optional': c.get('optional', False)}
        checks.append(sc)
    cfg = pf['config']
    return {'config': {'content_server': cfg['content_server'],
                       'ab_cdn': cfg['ab_cdn']},
            'checks': checks, 'runs': pf['runs'], 'ok': pf['ok']}

RATE_WINDOW = _env_int('ABGEN_RATE_WINDOW_S', 60)
RATE_MAX_HEAVY = _env_int('ABGEN_RATE_MAX', 20)
RATE_MAX_LIGHT = _env_int('ABGEN_RATE_MAX_LIGHT', 120)
HEAVY_SLOTS = _env_int('ABGEN_HEAVY_CONCURRENCY', 3)

def _parse_nets(spec):
    nets = []
    for tok in (spec or '').replace(',', ' ').split():
        try:
            nets.append(ipaddress.ip_network(tok, strict=False))
        except ValueError:
            pass
    return nets

RATE_TRUSTED_NETS = _parse_nets(os.environ.get('ABGEN_RATE_TRUSTED_NETS', ''))

def _rate_exempt_ip(ip_str):
    if not RATE_TRUSTED_NETS:
        return False
    try:
        ip = ipaddress.ip_address(ip_str.split('%')[0])
    except ValueError:
        return False
    return any(ip in n for n in RATE_TRUSTED_NETS)

HEAVY_PATHS = ('/api/bytediff', '/api/dump', '/api/preflight')
LIGHT_PATHS = ('/api/xxd', '/api/inv')

_rl_lock = threading.Lock()
_rl_hits = {}
_heavy = threading.BoundedSemaphore(HEAVY_SLOTS)

def rate_limited(key, limit):
    """Seconds to wait (0 = allowed) for one more hit in the sliding window."""
    now = time.monotonic()
    with _rl_lock:
        if key not in _rl_hits and len(_rl_hits) > 4096:
            _rl_hits.clear()
        dq = _rl_hits.setdefault(key, deque())
        while dq and now - dq[0] > RATE_WINDOW:
            dq.popleft()
        if len(dq) >= limit:
            return max(1, int(RATE_WINDOW - (now - dq[0])) + 1)
        dq.append(now)
        return 0

class H(SimpleHTTPRequestHandler):
    def __init__(self, *a, **k):
        super().__init__(*a, directory=SITE, **k)

    def log_message(self, *a):
        pass

    def end_headers(self):
        # crossOriginIsolated: the wasm converter's WebGPU bridge needs
        # SharedArrayBuffer + Atomics.wait in workers. Everything the site
        # loads is same-origin, so isolation costs nothing.
        self.send_header('Cross-Origin-Opener-Policy', 'same-origin')
        self.send_header('Cross-Origin-Embedder-Policy', 'require-corp')
        super().end_headers()

    def send_json(self, obj, code=200, headers=None):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def list_directory(self, path):
        self.send_error(404, 'File not found')
        return None

    def client_id(self):
        """(rate-limit key, is_local_operator). Loopback peers without a
        forwarding header are the operator on this box; a loopback peer WITH
        X-Forwarded-For is a same-host reverse proxy fronting the internet, so
        key by the forwarded address (last hop = the one our proxy appended)."""
        peer = self.client_address[0]
        try:
            loop = ipaddress.ip_address(peer.split('%')[0]).is_loopback
        except ValueError:
            loop = False
        if loop:
            fwd = self.headers.get('X-Forwarded-For', '').strip()
            if fwd:
                return fwd.split(',')[-1].strip()[:64], False
            return peer, True
        return peer, False

    def send_text(self, text, code=200):
        body = text.encode()
        self.send_response(code)
        self.send_header('Content-Type', 'text/plain; charset=utf-8')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def translate_path(self, path):
        p = urllib.parse.urlparse(path).path
        if p.startswith('/r/'):
            parts = p[3:].split('/', 1)
            run = urllib.parse.unquote(parts[0])
            rest = urllib.parse.unquote(parts[1]) if len(parts) > 1 else ''
            d = run_dir(run)
            if d:
                if rest in ('', 'data.json'):
                    return run_data_path(run) or os.path.join(d, 'site-data.json')
                fp = os.path.realpath(os.path.join(d, rest))
                if fp.startswith(os.path.realpath(d) + os.sep):
                    return fp
            return os.path.join(SITE, '.__nope__')
        return super().translate_path(path)

    def send_run_data(self, run):
        """Serve /r/<run>/data.json with ETag/Last-Modified + If-None-Match
        (the live index polls this; 304s keep the poll ~free). The ETag is
        weak-compare tolerant: fronting proxies (gzip/brotli) weaken it."""
        p = run_data_path(run)
        if not p:
            return self.send_json({'error': 'no such run'}, 404)
        try:
            st = os.stat(p)
        except OSError:
            return self.send_json({'error': 'no data'}, 404)
        etag = '"%x-%x"' % (st.st_mtime_ns, st.st_size)
        inm = self.headers.get('If-None-Match') or ''
        client_tags = [t.strip()[2:] if t.strip().startswith('W/') else t.strip()
                       for t in inm.split(',') if t.strip()]
        if etag in client_tags:
            self.send_response(304)
            self.send_header('ETag', etag)
            self.send_header('Cache-Control', 'no-cache')
            self.end_headers()
            return
        with open(p, 'rb') as f:
            body = f.read()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.send_header('ETag', etag)
        self.send_header('Last-Modified', self.date_time_string(st.st_mtime))
        self.send_header('Cache-Control', 'no-cache')
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        u = urllib.parse.urlparse(self.path)
        q = dict(urllib.parse.parse_qsl(u.query))
        try:
            key, local = self.client_id()
            if not local and not _rate_exempt_ip(key):
                limit = (RATE_MAX_HEAVY if u.path in HEAVY_PATHS
                         else RATE_MAX_LIGHT if u.path in LIGHT_PATHS else 0)
                if limit:
                    wait = rate_limited((key, u.path in HEAVY_PATHS), limit)
                    if wait:
                        return self.send_json({'error': 'rate limited'}, 429,
                                              {'Retry-After': str(wait)})
            if u.path.startswith('/r/'):
                parts = u.path[3:].split('/', 1)
                rest = urllib.parse.unquote(parts[1]) if len(parts) > 1 else ''
                if rest in ('', 'data.json'):
                    return self.send_run_data(urllib.parse.unquote(parts[0]))
            if u.path == '/':
                dest = '/index.html' if list_runs() else '/wizard.html'
                self.send_response(302)
                self.send_header('Location', dest)
                self.end_headers()
                return
            if u.path == '/api/runs':
                return self.send_json({'runs': list_runs(), 'lod': list_lod_runs()})
            if u.path == '/api/preflight':
                if not _heavy.acquire(timeout=10):
                    return self.send_json({'error': 'busy'}, 503)
                try:
                    pf = preflight(q.get('content') or DEFAULT_CONTENT,
                                   q.get('abcdn') or DEFAULT_ABCDN)
                finally:
                    _heavy.release()
                return self.send_json(pf if local else sanitize_preflight(pf))
            if u.path == '/api/row':
                run = q.get('run', '')
                ent, row = find_row(run, q.get('plat'), q.get('pair'))
                if not row:
                    return self.send_json({'error': 'not found'}, 404)
                p = run_data_path(run)
                d = cached_json(p) if p else {}
                return self.send_json({'entity': ent, 'row': row,
                                       'labels': (d or {}).get('labels'),
                                       'thresholds': (d or {}).get('thresholds')})
            if u.path == '/api/locate':
                return self.send_json(locate(q.get('run', ''), q.get('plat'), q.get('pair')))
            if u.path == '/api/bytediff':
                a, b = safe(q.get('a')), safe(q.get('b'))
                if not a or not b:
                    return self.send_json({'error': 'path not allowed/found'}, 400)
                if not _heavy.acquire(timeout=10):
                    return self.send_json({'error': 'busy'}, 503)
                try:
                    return self.send_json(bytediff(a, b))
                finally:
                    _heavy.release()
            if u.path == '/api/xxd':
                f = safe(q.get('f'))
                if not f:
                    return self.send_json({'error': 'path not allowed/found'}, 400)
                try:
                    off = max(0, int(q.get('off', 0)))
                    ln = min(4096, max(16, int(q.get('len', 1024))))
                except ValueError:
                    return self.send_json({'error': 'bad request'}, 400)
                return self.send_text(xxd(f, off, ln))
            if u.path == '/api/dump':
                f = safe(q.get('f'))
                tool = TOOLS.get(q.get('tool', ''))
                if not f or not tool or not os.path.isfile(tool):
                    return self.send_json({'error': 'bad tool or path'}, 400)
                if os.path.getsize(f) > MAX_DUMP_INPUT:
                    return self.send_text(
                        f'[file too large for dump tools: >{MAX_DUMP_INPUT} bytes]', 400)
                if not _heavy.acquire(timeout=10):
                    return self.send_json({'error': 'busy'}, 503)
                try:
                    r = subprocess.run([tool, f], capture_output=True, text=True, timeout=45)
                    out = (r.stdout or '') + (('\n[stderr]\n' + r.stderr) if r.returncode else '')
                    if len(out) > MAX_TEXT_OUT:
                        out = out[:MAX_TEXT_OUT] + f'\n[truncated at {MAX_TEXT_OUT} bytes]'
                    return self.send_text(out)
                except subprocess.TimeoutExpired:
                    return self.send_text('[timeout after 45s]', 504)
                finally:
                    _heavy.release()
            if u.path == '/api/inv':
                f = safe(q.get('f'))
                if not f:
                    return self.send_json({'error': 'path not allowed/found'}, 400)
                with open(f) as fh:
                    body = fh.read(MAX_TEXT_OUT + 1)
                if len(body) > MAX_TEXT_OUT:
                    body = body[:MAX_TEXT_OUT] + f'\n[truncated at {MAX_TEXT_OUT} bytes]'
                return self.send_text(body)
        except Exception as e:
            print(f'{self.path}: {type(e).__name__}: {e}', file=sys.stderr)
            return self.send_json({'error': 'internal error'}, 500)
        return super().do_GET()

if __name__ == '__main__':
    args, skip = [], False
    host = os.environ.get('ABGEN_SITE_HOST') or '0.0.0.0'
    argv = sys.argv[1:]
    i = 0
    while i < len(argv):
        a = argv[i]
        if a == '--runs':
            i += 2
            continue
        if a == '--host':
            host = argv[i + 1] if i + 1 < len(argv) else host
            i += 2
            continue
        args.append(a)
        i += 1
    port = int(args[0]) if args and args[0].isdigit() else 5197
    try:
        httpd = ThreadingHTTPServer((host, port), H)
    except OSError as e:
        sys.exit(f'cannot bind {host}:{port}: {e} — pass another port, e.g. '
                 f'abgen-compare serve {port + 100}')
    print(f'abgen compare-site on http://{host}:{port}  runs={RUNS}  '
          f'({len(list_runs())} run(s))', flush=True)
    httpd.serve_forever()
