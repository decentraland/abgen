"""Shared per-entity headless analysis: pairing rows -> classified matrix rows.

Extracted from the CLI's run stage 5 so one-shot runs (`abgen-compare run`)
and the live JIT watch loop (`abgen-compare watch`) share the exact same
verdict logic: byte diff, pid/CAB-normalized structure diff, texture
decode-compare. Verdict labels come from classify.py (the spec).
"""
import os

from .analyze import bytediff, struct_diff, texture_compare
from .classify import headless_glb_class, texture_label


def split_pairs(pairs):
    """paired rows -> (comparable, purged). Mirrors the run-mode rules:
    comparable = both sides on disk; purged = paired but a side's payload is
    gone (upstream purge) — those become explicit skipped/fail rows."""
    comparable = [
        r for r in pairs
        if r["status"] == "paired" and r["ours_path"] and r["upstream_path"]
    ]
    purged = [
        r for r in pairs
        if r["status"] == "paired" and not (r["ours_path"] and r["upstream_path"])
    ]
    return comparable, purged


def headless_matrix_rows(run_dir, run_id, platform, pairs, log, ts):
    """Run the headless stages over one pairing result.

    -> (matrix_rows, byte_rows, struct_rows); matrix rows carry the six
    display labels (+ explicit skipped rows for purged pairs). No renders.
    """
    comparable, purged = split_pairs(pairs)
    matrix, byte_rows, struct_rows = [], [], []
    tex_pairs = [r for r in comparable if r["kind"] == "texture"]
    tex_rows = texture_compare(run_dir, run_id, tex_pairs, log)
    for r in comparable:
        pid = r["pair_id"]
        opath = os.path.join(run_dir, r["ours_path"])
        upath = os.path.join(run_dir, r["upstream_path"])
        byte = bytediff(upath, opath)
        byte_rows.append({"pair": pid, **byte})
        struct = struct_diff(upath, opath)
        struct_rows.append({"pair": pid, **struct})
        base = {
            "pair": pid, "rev": 1, "run": run_id, "entity": r["entity"],
            "bundle": r["cid"], "platform": platform, "kind": r["kind"],
            "chunk": "headless", "upstreamVersion": r.get("manifest_version"),
            "oursSource": r["ours_source"], "bytesProv": "jit",
            "byte": {k: byte[k] for k in
                     ("identical", "sizeA", "sizeB", "pctDiff", "firstDiff")},
            "struct": {k: v for k, v in struct.items() if k != "samples"},
            "ts": ts,
        }
        if r["kind"] == "texture" and pid in tex_rows:
            tr = tex_rows[pid]
            base.update({
                "class": tr.get("class"),
                "label": texture_label(tr),
                "ppm": tr.get("ppm"),
                "maxd": tr.get("maxChannelDelta"),
                "corpusStub": tr.get("corpusStub", False),
                "texCount": tr.get("texCount"),
                "notes": tr.get("notes") or [],
            })
        else:
            cls, label, notes = headless_glb_class(byte, struct)
            base.update({"class": cls, "label": label, "notes": notes})
        matrix.append(base)
    for r in purged:
        side = "upstream" if not r["upstream_path"] else "ours"
        matrix.append({
            "pair": r["pair_id"], "rev": 1, "run": run_id, "entity": r["entity"],
            "bundle": r["cid"], "platform": platform, "kind": r["kind"],
            "chunk": "headless", "upstreamVersion": r.get("manifest_version"),
            "oursSource": r["ours_source"], "bytesProv": "jit",
            "class": "skipped", "label": "fail",
            "notes": [f"{side} payload purged/unfetchable — name-only pair"],
            "ts": ts,
        })
    return matrix, byte_rows, struct_rows
