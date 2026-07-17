"""Headless analysis (ALWAYS runs): byte diff, objdump structural diff
(pid/CAB-normalized), texture decode-compare (texcmp) + mip0 PNGs (texpng).

Tools come from the repo's release build:
  target/release/examples/{objdump,texdump,matdump,texcmp,texpng}
"""
import hashlib
import json
import os
import re
import subprocess
from collections import Counter

from .localserver import REPO_ROOT

TOOL_DIR = os.path.join(REPO_ROOT, "target", "release", "examples")

CAB_RE = re.compile(r"cab-[0-9a-f]{32}", re.IGNORECASE)
PID_RE = re.compile(r"pid ?= ?-?\d+")
PID_RAW_RE = re.compile(r"pid ?= ?(-?\d+)")
PINNED_RE = re.compile(r"^class=142 |^class=49 pid=※ name=metadata( |$)")


def vol_norm(line):
    return PID_RE.sub("pid=※", CAB_RE.sub("CAB-…", line))


def signature_of(norm_line):
    """Stable truncated key for one normalized diff line: whitespace collapsed
    and digit runs folded to '#', so one divergence CLASS maps to one
    allowlist signature instead of one entry per literal line."""
    return re.sub(r"\d+", "#", " ".join(norm_line.split()))[:96]


def tool(name):
    """Resolve an analysis tool binary. Precedence: ABGEN_<NAME> env (exact
    path) > ABGEN_TOOLS_DIR > target/release/examples > result/bin (nix)."""
    p = os.environ.get("ABGEN_" + name.upper())
    if p and os.path.isfile(p) and os.access(p, os.X_OK):
        return p
    for d in (os.environ.get("ABGEN_TOOLS_DIR"), TOOL_DIR,
              os.path.join(REPO_ROOT, "result", "bin")):
        if not d:
            continue
        p = os.path.join(d, name)
        if os.path.isfile(p) and os.access(p, os.X_OK):
            return p
    return None

def bytediff(a_path, b_path, max_ranges=32):
    ra = open(a_path, "rb").read()
    rb = open(b_path, "rb").read()
    out = {
        "sizeA": len(ra),
        "sizeB": len(rb),
        "sha256A": hashlib.sha256(ra).hexdigest(),
        "sha256B": hashlib.sha256(rb).hexdigest(),
        "identical": ra == rb,
    }
    if out["identical"]:
        out.update({"firstDiff": -1, "differingBytes": 0, "pctDiff": 0.0, "ranges": []})
        return out
    n = min(len(ra), len(rb))
    try:
        import numpy as np

        da = np.frombuffer(ra[:n], dtype=np.uint8)
        db = np.frombuffer(rb[:n], dtype=np.uint8)
        neq = da != db
        diffs = int(neq.sum())
        idx = np.flatnonzero(neq)
        first = int(idx[0]) if idx.size else -1
        ranges = []
        if idx.size:
            breaks = np.flatnonzero(np.diff(idx) > 1)
            starts = np.concatenate(([0], breaks + 1))
            ends = np.concatenate((breaks, [idx.size - 1]))
            for s, e in zip(starts[:max_ranges], ends[:max_ranges]):
                ranges.append([int(idx[s]), int(idx[e])])
    except ImportError:
        first = -1
        diffs = 0
        ranges = []
        in_range = False
        start = 0
        CH = 65536
        for off in range(0, n, CH):
            ca, cb = ra[off:off + CH], rb[off:off + CH]
            if ca == cb:
                if in_range:
                    in_range = False
                    if len(ranges) < max_ranges:
                        ranges.append([start, off - 1])
                continue
            for i, (x, y) in enumerate(zip(ca, cb)):
                if x != y:
                    diffs += 1
                    if first < 0:
                        first = off + i
                    if not in_range:
                        in_range, start = True, off + i
                elif in_range:
                    in_range = False
                    if len(ranges) < max_ranges:
                        ranges.append([start, off + i - 1])
        if in_range and len(ranges) < max_ranges:
            ranges.append([start, n - 1])
    diffs += abs(len(ra) - len(rb))
    out.update(
        {
            "firstDiff": first,
            "differingBytes": diffs,
            "pctDiff": round(100 * diffs / max(1, max(len(ra), len(rb))), 3),
            "ranges": ranges,
        }
    )
    return out

def run_dump(tool_path, bundle_path, timeout=90, cap=400_000):
    r = subprocess.run(
        [tool_path, bundle_path], capture_output=True, text=True, timeout=timeout
    )
    out = (r.stdout or "") + (("\n[stderr]\n" + r.stderr) if r.returncode else "")
    return out[:cap], r.returncode


def _dump_lines(text):
    lines = []
    for raw in text.split("\n"):
        line = raw.strip()
        if not line or line.startswith("== "):
            continue
        lines.append(line)
    return lines


def dump_texts(up_path, ours_path, timeout=90):
    """objdump BOTH sides exactly once so struct_diff and id_diff share the
    captured raw texts. -> (fail_dict|None, up_txt, ours_txt)."""
    objdump = tool("objdump")
    if not objdump:
        return {"error": "objdump tool missing (build crate examples)"}, None, None
    try:
        up_txt, up_rc = run_dump(objdump, up_path, timeout)
    except subprocess.TimeoutExpired:
        return {"class": "loadFailUpstream",
                "error": "objdump timeout (upstream)"}, None, None
    try:
        ours_txt, ours_rc = run_dump(objdump, ours_path, timeout)
    except subprocess.TimeoutExpired:
        return {"class": "loadFailOurs",
                "error": "objdump timeout (ours)"}, None, None
    for side, txt, rc in (("Upstream", up_txt, up_rc), ("Ours", ours_txt, ours_rc)):
        if rc != 0 or "parse error" in txt or "read error" in txt:
            return {
                "class": f"loadFail{side}",
                "error": f"objdump failed on {side.lower()} bundle",
            }, up_txt, ours_txt
    return None, up_txt, ours_txt


def struct_diff_from_texts(up_txt, ours_txt):
    """pid/CAB-normalize, line-multiset diff. Counts split into pinned
    (manifest/metadata: expected to differ — timestamp, version, CAB names)
    and real structural drift; both get bounded samples."""
    ul = [vol_norm(x) for x in _dump_lines(up_txt)]
    ol = [vol_norm(x) for x in _dump_lines(ours_txt)]
    pinned_lines, samples, pinned_samples = 0, [], []
    up_only = ours_only = 0
    uset, oset = {}, {}
    for x in ul:
        uset[x] = uset.get(x, 0) + 1
    for x in ol:
        oset[x] = oset.get(x, 0) + 1
    for x, c in uset.items():
        extra = c - oset.get(x, 0)
        if extra > 0:
            if PINNED_RE.search(x):
                pinned_lines += extra
                if len(pinned_samples) < 8:
                    pinned_samples.append({"up": x[:200], "ours": None})
            else:
                up_only += extra
                if len(samples) < 12:
                    samples.append({"up": x[:200], "ours": None})
    for x, c in oset.items():
        extra = c - uset.get(x, 0)
        if extra > 0:
            if PINNED_RE.search(x):
                pinned_lines += extra
                if len(pinned_samples) < 8:
                    pinned_samples.append({"up": None, "ours": x[:200]})
            else:
                ours_only += extra
                if len(samples) < 12:
                    samples.append({"up": None, "ours": x[:200]})
    equal = up_only == 0 and ours_only == 0
    return {
        "class": "structIdentical" if equal else "structDiff",
        "linesUp": len(ul),
        "linesOurs": len(ol),
        "diffLines": up_only + ours_only,
        "pinnedDiffLines": pinned_lines,
        "upOnly": up_only,
        "oursOnly": ours_only,
        "samples": samples,
        "pinnedSamples": pinned_samples,
    }


def struct_diff(up_path, ours_path, timeout=90):
    """objdump both sides, pid/CAB-normalize, line-diff.

    Returns dict: parse failures -> loadFail*; else struct_diff_from_texts.
    """
    fail, up_txt, ours_txt = dump_texts(up_path, ours_path, timeout)
    if fail:
        return fail
    return struct_diff_from_texts(up_txt, ours_txt)


def id_diff(up_txt, ours_txt):
    """L3 identity agreement over RAW objdump text — zero normalization, the
    instrument vol_norm structurally erases: raw pid= i64 multisets (pid
    equality transitively tests GUID-seed agreement — path_id = prefab-packed
    SpookyHash of (guid, localId, fileType)) and raw CAB node names."""
    up_pids = Counter(int(m) for m in PID_RAW_RE.findall(up_txt))
    ours_pids = Counter(int(m) for m in PID_RAW_RE.findall(ours_txt))
    inter = sum((up_pids & ours_pids).values())
    union = sum((up_pids | ours_pids).values())
    up_only = up_pids - ours_pids
    ours_only = ours_pids - up_pids
    cab_up = sorted(set(CAB_RE.findall(up_txt)))
    cab_ours = sorted(set(CAB_RE.findall(ours_txt)))
    samples = [{"side": "up", "pid": p} for p in sorted(up_only)[:6]]
    samples += [{"side": "ours", "pid": p}
                for p in sorted(ours_only)[:12 - len(samples)]]
    return {
        "cabUp": cab_up,
        "cabOurs": cab_ours,
        "cabEqual": cab_up == cab_ours,
        "pidUp": sum(up_pids.values()),
        "pidOurs": sum(ours_pids.values()),
        "pidJaccard": round(inter / union, 6) if union else 1.0,
        "pidUpOnly": sum(up_only.values()),
        "pidOursOnly": sum(ours_only.values()),
        "samples": samples,
    }

def texture_compare(run_dir, run_id, tex_pairs, log):
    """tex_pairs: paired rows with kind == texture and both sides on disk.
    Runs texcmp (per-texture stats rows -> analysis/texcmp.jsonl) and texpng
    (mip0 PNGs -> tex-images/). Returns {pair_id: texcmp_row}."""
    if not tex_pairs:
        return {}
    texcmp = tool("texcmp")
    texpng = tool("texpng")
    if not texcmp or not texpng:
        log("analyze: WARN texcmp/texpng missing — texture pairs get structure/bytes only")
        return {}
    tasks_path = os.path.join(run_dir, "analysis", "texcmp-tasks.jsonl")
    out_path = os.path.join(run_dir, "analysis", "texcmp.jsonl")
    with open(tasks_path, "w") as f:
        for r in tex_pairs:
            f.write(json.dumps({
                "pair": r["pair_id"], "set": run_id, "entity": r["entity"],
                "bundle": r["cid"], "platform": r.get("platform"),
                "chunk": "headless",
                "oursSource": r["ours_source"],
                "upstreamVersion": r.get("manifest_version"),
                "ours": os.path.join(run_dir, r["ours_path"]),
                "upstream": os.path.join(run_dir, r["upstream_path"]),
            }) + "\n")
    subprocess.run([texcmp, tasks_path, out_path, "8"], check=True, timeout=600)
    tex_dir = os.path.join(run_dir, "tex-images")
    subprocess.run([texpng, tasks_path, tex_dir, "8"], check=True, timeout=600)
    for f in os.listdir(tex_dir):
        if f.endswith("-abgen.png"):
            os.replace(os.path.join(tex_dir, f),
                       os.path.join(tex_dir, f[:-len("-abgen.png")] + "-ours.png"))
        elif f.endswith("-abgen.missing.txt"):
            os.replace(os.path.join(tex_dir, f),
                       os.path.join(tex_dir, f[:-len("-abgen.missing.txt")] + "-ours.missing.txt"))
    rows = {}
    for line in open(out_path):
        line = line.strip()
        if line:
            r = json.loads(line)
            rows[r["pair"]] = r
    log(f"analyze: texcmp/texpng done for {len(rows)} texture pair(s)")
    return rows
