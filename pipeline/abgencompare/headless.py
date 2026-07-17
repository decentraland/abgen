"""Shared per-entity headless analysis: pairing rows -> classified matrix rows.

Extracted from the CLI's run stage 5 so one-shot runs (`abgen-compare run`)
and the live JIT watch loop (`abgen-compare watch`) share the exact same
verdict logic: byte diff, pid/CAB-normalized structure diff, texture
decode-compare. Verdict labels come from classify.py (the spec).
"""
import os

from .analyze import (bytediff, dump_texts, id_diff, struct_diff_from_texts,
                      texture_compare)
from .classify import headless_glb_class, texture_label
from .util import append_jsonl


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


def headless_matrix_rows(run_dir, run_id, platform, pairs, log, ts, tier="prod"):
    """Run the headless stages over one pairing result.

    -> (matrix_rows, byte_rows, struct_rows); matrix rows carry the six
    display labels (+ explicit skipped rows for purged pairs) and a tier
    stamp. Both sides are objdumped once; the raw texts feed struct_diff and
    id_diff (analysis/iddiff.jsonl — raw CAB/pid identity agreement, the L3
    instrument). No renders.
    """
    comparable, purged = split_pairs(pairs)
    matrix, byte_rows, struct_rows, id_rows = [], [], [], []
    tex_pairs = [r for r in comparable if r["kind"] == "texture"]
    tex_rows = texture_compare(run_dir, run_id, tex_pairs, log)
    for r in comparable:
        pid = r["pair_id"]
        opath = os.path.join(run_dir, r["ours_path"])
        upath = os.path.join(run_dir, r["upstream_path"])
        byte = bytediff(upath, opath)
        byte_rows.append({"pair": pid, **byte})
        fail, up_txt, ours_txt = dump_texts(upath, opath)
        struct = fail or struct_diff_from_texts(up_txt, ours_txt)
        struct_rows.append({"pair": pid, **struct})
        idd = None
        if not fail:
            idd = id_diff(up_txt, ours_txt)
            id_rows.append({"pair": pid, "run": run_id, "entity": r["entity"],
                            "bundle": r["cid"], "platform": platform,
                            "tier": tier, "ts": ts, **idd})
        base = {
            "pair": pid, "rev": 1, "run": run_id, "entity": r["entity"],
            "bundle": r["cid"], "platform": platform, "kind": r["kind"],
            "chunk": "headless", "tier": tier,
            "upstreamVersion": r.get("manifest_version"),
            "oursSource": r["ours_source"], "bytesProv": "jit",
            "byte": {k: byte[k] for k in
                     ("identical", "sizeA", "sizeB", "pctDiff", "firstDiff")},
            "struct": {k: v for k, v in struct.items()
                       if k not in ("samples", "pinnedSamples")},
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
        if tier == "local" and idd:
            base["notes"] = list(base.get("notes") or []) + [
                f"id-agree: cab={idd['cabEqual']} pidJ={idd['pidJaccard']}"]
        matrix.append(base)
    for r in purged:
        side = "upstream" if not r["upstream_path"] else "ours"
        matrix.append({
            "pair": r["pair_id"], "rev": 1, "run": run_id, "entity": r["entity"],
            "bundle": r["cid"], "platform": platform, "kind": r["kind"],
            "chunk": "headless", "tier": tier,
            "upstreamVersion": r.get("manifest_version"),
            "oursSource": r["ours_source"], "bytesProv": "jit",
            "class": "skipped", "label": "fail",
            "notes": [f"{side} payload purged/unfetchable — name-only pair"],
            "ts": ts,
        })
    if id_rows:
        append_jsonl(os.path.join(run_dir, "analysis", "iddiff.jsonl"), id_rows)
    return matrix, byte_rows, struct_rows
