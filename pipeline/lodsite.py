#!/usr/bin/env python3
"""LOD parity dataset builder for the compare site's /lod.html page.

Walks the published LOD tree (out_root/{sceneId}/LOD/1/{sceneId}_1_windows),
pairs each scene with its production counterpart from ab-cdn (cached, 404 =>
ours-only), runs `abgen-lod compare` for structural rows and `atlasprobe` for
atlas mip dumps + occupancy on both sides, and emits a self-contained run dir
the site serves statically:

  runs/<run-id>/
    lod-data.json          # everything lod.html renders
    img/<stem>-<tex>-mip{0,3,5}.png
    prod/<sceneId>_1_windows   # cached production bundles

No server changes needed: /r/<run-id>/<path> already serves run dirs, and
lod.html is a plain static page. Rerun to refresh; fetches are cached, probe
and compare outputs are recomputed.

Usage: lodsite.py [--run-id lod-YYYYMMDD] [--out-root DIR] [--runs DIR]
                  [--platform windows] [--jobs 8] [--limit N]
"""
import argparse, hashlib, json, os, re, subprocess, sys, time
import urllib.request, urllib.error
from concurrent.futures import ThreadPoolExecutor

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEF_OUT_ROOT = os.environ.get('ABGEN_LOD_OUT_ROOT') or os.path.join(REPO, 'out')
DEF_RUNS = os.environ.get('ABGEN_RUNS_DIR') or os.path.join(REPO, 'runs')
PROD_BASE = os.environ.get('ABGEN_LOD_PROD_BASE') or 'https://ab-cdn.decentraland.org/LOD/1/'
CONTENT_LOCAL = os.environ.get('ABGEN_LOD_CONTENT_URL') or 'http://127.0.0.1:5141/contents/'
UA = {'User-Agent': 'curl/8.9 (lodsite dataset builder)'}


def find_tool(env, *cands):
    p = os.environ.get(env)
    if p:
        return p
    for c in cands:
        c = os.path.normpath(os.path.join(REPO, c))
        if os.path.isfile(c):
            return c
    return cands[0]


ABGEN_LOD = find_tool('ABGEN_LOD_BIN',
                      'target/release/abgen-lod',
                      '../../target/release/abgen-lod')
ATLASPROBE = find_tool('ATLASPROBE_BIN',
                       'target/release/examples/atlasprobe',
                       '../../target/release/examples/atlasprobe')

ROW_RE = re.compile(r'^(PASS|FAIL|INFO) ([^:]+): ?(.*)$')
NUM_RE = re.compile(r'verts=(\d+) idxfmt=\d+ tris=(\d+)')
TEX_RE = re.compile(r'^Texture2D name=(.+) fmt=(-?\d+) (\d+)x(\d+) mipCount=(\d+)')
MIP_RE = re.compile(r'^\s+mip(\d+) (\d+)x(\d+) meanRGBA=\(([^)]+)\) meanLuma=([\d.]+).*nearBlack=([\d.]+)')
OCC_RE = re.compile(r'occupancy\(vs [^)]+\)=([\d.]+) bgFrac=([\d.]+)')


def sha256(path):
    h = hashlib.sha256()
    with open(path, 'rb') as f:
        for chunk in iter(lambda: f.read(1 << 20), b''):
            h.update(chunk)
    return h.hexdigest()


def fetch_prod(scene, platform, dest):
    if os.path.isfile(dest) and os.path.getsize(dest) > 0:
        return True
    url = f'{PROD_BASE}{scene}_1_{platform}'
    try:
        req = urllib.request.Request(url, headers=UA)
        with urllib.request.urlopen(req, timeout=60) as r:
            data = r.read()
        tmp = dest + '.tmp'
        with open(tmp, 'wb') as f:
            f.write(data)
        os.replace(tmp, dest)
        return True
    except urllib.error.HTTPError as e:
        if e.code in (403, 404):
            return False
        print(f'  {scene}: prod fetch HTTP {e.code}', file=sys.stderr)
        return False
    except Exception as e:
        print(f'  {scene}: prod fetch failed: {e}', file=sys.stderr)
        return False


def entity_meta(scene):
    try:
        req = urllib.request.Request(CONTENT_LOCAL + scene, headers=UA)
        with urllib.request.urlopen(req, timeout=10) as r:
            ent = json.load(r)
        md = ent.get('metadata') or {}
        disp = md.get('display') or {}
        return {
            'title': disp.get('title') or '',
            'pointers': ent.get('pointers') or [],
            'base': (md.get('scene') or {}).get('base') or '',
        }
    except Exception:
        return {'title': '', 'pointers': [], 'base': ''}


def run_compare(ours, prod):
    p = subprocess.run([ABGEN_LOD, 'compare', ours, prod],
                       capture_output=True, text=True, timeout=300)
    rows, fails = [], 0
    for line in p.stdout.splitlines():
        m = ROW_RE.match(line)
        if not m:
            continue
        s, name, detail = m.groups()
        rows.append({'s': s, 'name': name, 'detail': detail})
        if s == 'FAIL':
            fails += 1
    return rows, fails


def mesh_totals(rows, side):
    verts = tris = 0
    for r in rows:
        if not r['name'].startswith('mesh['):
            continue
        nums = NUM_RE.findall(r['detail'])
        if not nums:
            continue
        if side == 'ours':
            v, t = nums[0]
        else:
            v, t = nums[-1]
        verts += int(v)
        tris += int(t)
    return verts, tris


def probe(bundle, imgdir):
    p = subprocess.run([ATLASPROBE, 'bundle', bundle, imgdir],
                       capture_output=True, text=True, timeout=300)
    stem = os.path.basename(bundle)
    textures, cur = [], None
    verts = tris = 0
    for line in p.stdout.splitlines():
        m = TEX_RE.match(line)
        if m:
            cur = {'name': m.group(1), 'fmt': int(m.group(2)),
                   'w': int(m.group(3)), 'h': int(m.group(4)),
                   'mips': int(m.group(5)), 'levels': [], 'occupancy': None,
                   'imgs': {}}
            textures.append(cur)
            continue
        m = MIP_RE.match(line)
        if m and cur is not None:
            cur['levels'].append({'i': int(m.group(1)),
                                  'w': int(m.group(2)), 'h': int(m.group(3)),
                                  'meanLuma': float(m.group(5)),
                                  'nearBlack': float(m.group(6))})
            continue
        m = OCC_RE.search(line)
        if m and cur is not None:
            cur['occupancy'] = float(m.group(1))
        m = re.search(r'total verts=(\d+) tris=(\d+)', line)
        if m:
            verts, tris = int(m.group(1)), int(m.group(2))
    for t in textures:
        safe = re.sub(r'[^A-Za-z0-9_.-]', '_', t['name'])
        for i in (0, 3, 5):
            f = f'{stem}-{safe}-mip{i}.png'
            if os.path.isfile(os.path.join(imgdir, f)):
                t['imgs'][f'mip{i}'] = f
    return {'textures': textures, 'probe_verts': verts, 'probe_tris': tris}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--run-id', default=time.strftime('lod-%Y%m%d'))
    ap.add_argument('--out-root', default=DEF_OUT_ROOT)
    ap.add_argument('--runs', default=DEF_RUNS)
    ap.add_argument('--platform', default='windows')
    ap.add_argument('--jobs', type=int, default=8)
    ap.add_argument('--limit', type=int, default=0)
    ap.add_argument('--renders-dir', default='',
                    help='dir with <scene>-{ours,prod}-a0.png Unity renders to embed')
    args = ap.parse_args()

    for tool in (ABGEN_LOD, ATLASPROBE):
        if not os.path.isfile(tool):
            sys.exit(f'missing tool {tool} — build with: cargo build --release '
                     f'-p abgen --bin abgen-lod --example atlasprobe')

    rundir = os.path.join(args.runs, args.run_id)
    imgdir = os.path.join(rundir, 'img')
    proddir = os.path.join(rundir, 'prod')
    os.makedirs(imgdir, exist_ok=True)
    os.makedirs(proddir, exist_ok=True)

    scenes = []
    for name in sorted(os.listdir(args.out_root)):
        b = os.path.join(args.out_root, name, 'LOD', '1',
                         f'{name}_1_{args.platform}')
        if os.path.isfile(b):
            scenes.append((name, b))
    if args.limit:
        scenes = scenes[:args.limit]
    print(f'{len(scenes)} scenes with published LOD1 ({args.platform})')

    with ThreadPoolExecutor(args.jobs) as ex:
        prod_ok = dict(zip(
            [s for s, _ in scenes],
            ex.map(lambda sb: fetch_prod(sb[0], args.platform,
                                         os.path.join(proddir, f'{sb[0]}_1_{args.platform}')),
                   scenes)))
        metas = dict(zip([s for s, _ in scenes],
                         ex.map(lambda sb: entity_meta(sb[0]), scenes)))

    out_scenes = []
    for i, (scene, ours) in enumerate(scenes):
        prod = os.path.join(proddir, f'{scene}_1_{args.platform}')
        has_prod = prod_ok.get(scene) and os.path.isfile(prod)
        row = {'id': scene, **metas[scene],
               'ours': {'size': os.path.getsize(ours), 'sha': sha256(ours)[:16]},
               'prod': None, 'checks': [], 'fails': None}
        row['ours'].update(probe(ours, imgdir))
        if row['ours'].get('probe_tris'):
            row['ours']['tris'] = row['ours']['probe_tris']
        if has_prod:
            row['prod'] = {'size': os.path.getsize(prod), 'sha': sha256(prod)[:16]}
            row['prod'].update(probe(prod, imgdir))
            rows, fails = run_compare(ours, prod)
            row['checks'], row['fails'] = rows, fails
            ov, ot = mesh_totals(rows, 'ours')
            pv, pt = mesh_totals(rows, 'prod')
            row['ours'].update({'verts': ov, 'tris': ot})
            row['prod'].update({'verts': pv, 'tris': pt})
        if args.renders_dir:
            for side in ('ours', 'prod'):
                src = os.path.join(args.renders_dir, f'{scene}-{side}-a0.png')
                if row[side] is not None and os.path.isfile(src):
                    fname = f'{scene}-{side}-render.png'
                    with open(src, 'rb') as fi, open(os.path.join(imgdir, fname), 'wb') as fo:
                        fo.write(fi.read())
                    row[side]['render'] = fname
        out_scenes.append(row)
        if (i + 1) % 10 == 0:
            print(f'  {i + 1}/{len(scenes)}')

    with_prod = sum(1 for r in out_scenes if r['prod'])
    data = {
        'generated': time.strftime('%Y-%m-%dT%H:%M:%S%z'),
        'platform': args.platform,
        'ours_base': '/LOD/1/',
        'prod_base': PROD_BASE,
        'stats': {'scenes': len(out_scenes), 'with_prod': with_prod,
                  'ours_only': len(out_scenes) - with_prod},
        'scenes': out_scenes,
    }
    dest = os.path.join(rundir, 'lod-data.json')
    with open(dest + '.tmp', 'w') as f:
        json.dump(data, f, separators=(',', ':'))
    os.replace(dest + '.tmp', dest)
    print(f'wrote {dest} ({len(out_scenes)} scenes, {with_prod} with prod)')


if __name__ == '__main__':
    main()
