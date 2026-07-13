"""Per-run site-data.json builder (canonical run-model contract, served by
site/server.py as /r/<run-id>/data.json).

  {run, description, generated, labels:{upstream,ours}, thresholds,
   entities:[{entity,cat,rows:[{pair,platform,tags[],bundle,kind,cat,label,
     detail,prov,ppm,ppm8,maxd,upVersion,up:[run-relative]|null,
     ours:[run-relative]|null,gifs,note}]}],
   stats:{entities,pairs,labels{six},platforms,cats,kinds,tags}}

Side names are canonical `up` / `ours`; shot paths are run-relative
(renders/<pair>-<side>-a<i>.png, tex-images/<pair>-<side>.png) — no symlinks.
"""
import collections
import datetime
import os

from . import LABELS
from .util import read_jsonl


def newest_rows(matrix_rows):
    by_pair = {}
    for r in matrix_rows:
        prev = by_pair.get(r["pair"])
        if prev is None or r.get("rev", 1) >= prev.get("rev", 1):
            by_pair[r["pair"]] = r
    return by_pair


def _shots(run_dir, pair, side, kind, rendered):
    """Existing shot list for one side, run-relative, or None."""
    if kind == "texture":
        name = f"tex-images/{pair}-{side}.png"
        return [name] if os.path.exists(os.path.join(run_dir, name)) else None
    if not rendered:
        return None
    names = [f"renders/{pair}-{side}-a{i}.png" for i in range(3)]
    if all(os.path.exists(os.path.join(run_dir, n)) for n in names):
        return names
    return None


def build_run_sitedata(run_dir, run_id, config=None):
    config = config or {}
    matrix = read_jsonl(os.path.join(run_dir, "analysis", "matrix.jsonl"))
    meta = {m["entity"]: m for m in read_jsonl(os.path.join(run_dir, "entity-meta.jsonl"))}
    tags = list(config.get("tags") or [])
    entities = collections.defaultdict(list)
    for pid, r in sorted(newest_rows(matrix).items()):
        notes = [str(n) for n in (r.get("notes") or [])]
        if r.get("class") == "skipped" and any("purged" in n for n in notes):
            continue
        rendered = r.get("chunk") == "render"
        cat = (meta.get(r["entity"]) or {}).get("type", "unknown")
        gifs = {}
        for side in ("up", "ours"):
            g = f"renders/{pid}-{side}.gif"
            if os.path.exists(os.path.join(run_dir, g)):
                gifs[side] = g
        entities[r["entity"]].append({
            "pair": pid,
            "platform": r["platform"],
            "tags": tags + ([r["chunk"]] if r.get("chunk") else []),
            "bundle": r["bundle"],
            "kind": r["kind"],
            "cat": cat,
            "label": r["label"],
            "detail": r["class"] + (" +stub" if r.get("corpusStub") else ""),
            "prov": r.get("prov") or "honest jit bytes (local abgen server)",
            "ppm": r.get("ppm"),
            "ppm8": r.get("ppm8"),
            "maxd": r.get("maxd"),
            "upVersion": r.get("upstreamVersion"),
            "up": _shots(run_dir, pid, "up", r["kind"], rendered),
            "ours": _shots(run_dir, pid, "ours", r["kind"], rendered),
            "gifs": gifs or None,
            "note": (notes[0][:120] if notes else ""),
        })
    all_rows = [x for v in entities.values() for x in v]
    return {
        "run": run_id,
        "description": config.get("description")
        or f"abgen-compare run: pointer={config.get('pointer')!r} "
           f"platform={config.get('platform')}",
        "generated": datetime.datetime.now(datetime.timezone.utc).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        ),
        "labels": config.get("labels") or {"upstream": "upstream ab-cdn", "ours": "abgen"},
        "thresholds": config.get("thresholds")
        or {"imperceptible_ppm": 200, "amnesty": {"delta_gt": 8, "ppm": 200}},
        "render": config.get("render_provenance"),
        "entities": [
            {
                "entity": e,
                "cat": (meta.get(e) or {}).get("type", "unknown"),
                "rows": sorted(v, key=lambda x: x["pair"]),
            }
            for e, v in sorted(entities.items())
        ],
        "stats": {
            "entities": len(entities),
            "pairs": len(all_rows),
            "labels": {l: sum(1 for x in all_rows if x["label"] == l) for l in LABELS},
            "platforms": dict(collections.Counter(x["platform"] for x in all_rows)),
            "cats": dict(collections.Counter(x["cat"] for x in all_rows)),
            "kinds": dict(collections.Counter(x["kind"] for x in all_rows)),
            "tags": dict(
                collections.Counter(t for x in all_rows for t in (x["tags"] or []))
            ),
        },
    }
