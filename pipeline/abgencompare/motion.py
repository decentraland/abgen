"""Motion-parity metrics from AbAnimCapture frame series.

For every animated pair with both sides' frame series in renders/
(<pair>-{ours,up}-fNN.png), computes per-frame ours-vs-up mean absolute
error plus each side's motion energy (mean abs diff between consecutive
frames) and writes analysis/motion.jsonl. Matching series with low MAE =
motion parity; near-zero energy on exactly one side = that side's clips
failed to sample (static while the other animates).
"""
import os


def _arr(path):
    import numpy as np
    from PIL import Image
    return np.asarray(Image.open(path).convert("RGB"), dtype=np.int16)


def _mean(xs):
    return round(sum(xs) / len(xs), 3) if xs else None


def motion_rows(run_dir, pairs, max_frames=64):
    import numpy as np
    rdir = os.path.join(run_dir, "renders")
    rows = []
    for r in pairs:
        if r["kind"] != "animated":
            continue
        pid = r["pair_id"]
        row = {"pair": pid, "entity": r["entity"], "bundle": r["cid"],
               "frames": 0, "maeMean": None, "maeMax": None,
               "energyOurs": None, "energyUp": None, "notes": []}
        ours, ups = [], []
        for k in range(max_frames):
            po = os.path.join(rdir, f"{pid}-ours-f{k:02d}.png")
            pu = os.path.join(rdir, f"{pid}-up-f{k:02d}.png")
            if not (os.path.isfile(po) and os.path.isfile(pu)):
                break
            ours.append(_arr(po))
            ups.append(_arr(pu))
        if not ours:
            row["notes"].append("no frame series on one or both sides")
            rows.append(row)
            continue
        maes = [float(np.abs(o - u).mean()) for o, u in zip(ours, ups)]
        eo = [float(np.abs(a - b).mean()) for a, b in zip(ours, ours[1:])]
        eu = [float(np.abs(a - b).mean()) for a, b in zip(ups, ups[1:])]
        row.update(frames=len(ours), maeMean=_mean(maes),
                   maeMax=round(max(maes), 3),
                   energyOurs=_mean(eo) or 0.0, energyUp=_mean(eu) or 0.0)
        if row["energyUp"] > 0.5 and row["energyOurs"] < 0.05:
            row["notes"].append("ours static while upstream animates")
        if row["energyOurs"] > 0.5 and row["energyUp"] < 0.05:
            row["notes"].append("upstream static while ours animates")
        rows.append(row)
    return rows
