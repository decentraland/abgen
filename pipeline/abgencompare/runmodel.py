"""Run directory model.

runs/<run-id>/            run-id = <UTC ts>-<slug>; immutable once COMPLETE exists
├── config.json           everything user-chosen (the only input)
├── run.log               stage journal
├── entity-meta.jsonl     {entity,type,pointers,content{path→cid},metadata_keys}
├── manifests/<entity>.{ours,upstream}.json
├── ours/<entity>/<platform>/<bundleFile>       bytes fetched from the local abgen server
├── upstream/<entity>/<platform>/<bundleFile>   bytes fetched from ab-cdn (kept)
├── ours-out/ ours-cache/                       scratch roots for a spawned abgen server
├── renders/<pair>-{up,ours}-a<i>.png (+ -anim.png, .inventory.json, .FAILED.txt)
├── tex-images/<pair>-{up,abgen}.png            decoded mip0 (texture pairs, texpng)
├── analysis/
│   ├── pairs.jsonl       pairing result (run-relative paths)
│   ├── bytediff.jsonl    per-pair byte diff
│   ├── structure.jsonl   per-pair objdump structural diff (pid/CAB-normalized)
│   ├── texcmp.jsonl      raw texcmp rows (texture pairs)
│   ├── amp-metrics.json  render amplitude bands  {"<pair>":{"a0":[...],...,"wh":[W,H]}}
│   └── matrix.jsonl      APPEND-ONLY classified rows; newest-wins per pair (rev)
├── jobs/                 harness inputs + logs (render mode)
├── site-data.json        site rows for this run
└── COMPLETE              immutability marker (refuse to touch the run once present)
"""
import datetime
import json
import os
import re

DEFAULT_RUNS_ROOT = os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "runs"
)

SUBDIRS = ("manifests", "ours", "upstream", "renders", "tex-images", "analysis", "jobs")


def runs_root(cli_value=None):
    return os.path.abspath(
        cli_value
        or os.environ.get("ABGEN_RUNS_DIR")
        or os.environ.get("ABGEN_COMPARE_RUNS")
        or DEFAULT_RUNS_ROOT
    )


def slugify(s):
    s = re.sub(r"[^A-Za-z0-9._-]+", "-", s).strip("-")
    return (s or "run")[:48]


def new_run_dir(root, slug):
    ts = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%d-%H%M%S")
    run_id = f"{ts}-{slugify(slug)}"
    run_dir = os.path.join(root, run_id)
    os.makedirs(run_dir)
    for d in SUBDIRS:
        os.makedirs(os.path.join(run_dir, d))
    return run_id, run_dir


def mark_complete(run_dir):
    with open(os.path.join(run_dir, "COMPLETE"), "w") as f:
        f.write(datetime.datetime.now(datetime.timezone.utc).isoformat() + "\n")


def is_complete(run_dir):
    return os.path.exists(os.path.join(run_dir, "COMPLETE"))


def check_mutable(run_dir):
    if is_complete(run_dir):
        raise SystemExit(
            f"run {run_dir} is COMPLETE (immutable) — re-runs make new run dirs"
        )


def list_runs(root):
    out = []
    if not os.path.isdir(root):
        return out
    for name in sorted(os.listdir(root)):
        d = os.path.join(root, name)
        if not os.path.isdir(d) or not os.path.exists(os.path.join(d, "config.json")):
            continue
        try:
            cfg = json.load(open(os.path.join(d, "config.json")))
        except (OSError, ValueError):
            cfg = {}
        stats = None
        sd = os.path.join(d, "site-data.json")
        if os.path.exists(sd):
            try:
                stats = json.load(open(sd)).get("stats")
            except (OSError, ValueError):
                pass
        out.append(
            {
                "run_id": name,
                "dir": d,
                "complete": is_complete(d),
                "platform": cfg.get("platform"),
                "pointer": cfg.get("pointer"),
                "created": cfg.get("created"),
                "rendered": cfg.get("rendered", False),
                "stats": stats,
            }
        )
    return out
