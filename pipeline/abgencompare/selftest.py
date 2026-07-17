"""In-run negative-control battery (M8): prove the comparators FAIL on
known-dissimilar inputs before any tier's green is trusted. All controls
operate on temp copies under <run>/jobs/selftest/ — fetched originals are
never touched. NC4 needs the other platform's flavor of one cid; it is
fetched from the run's ours server if still reachable, else the control is
recorded skipped-with-reason (never silently green — the verdict gate treats
a skip as a failure to prove falsifiability).
"""
import json
import os
import shutil

from .analyze import bytediff, dump_texts, id_diff, struct_diff, struct_diff_from_texts
from .fetch import norm_bundle_name
from .util import http_get


def _control(name, expected, observed, detected, **extra):
    return {"name": name, "expected": expected, "observed": observed,
            "detected": detected, **extra}


def _skip(name, expected, reason):
    return _control(name, expected, None, None, skipped=True, reason=reason)


def _copy(src, scratch, name):
    dst = os.path.join(scratch, name)
    shutil.copyfile(src, dst)
    return dst


def _pick_nc1(comp):
    """Two rows with distinct cids, cross-entity when possible — different
    content is guaranteed by content addressing."""
    by_cid = {}
    for r in comp:
        by_cid.setdefault(r["cid"], r)
    distinct = list(by_cid.values())
    if len(distinct) < 2:
        return None, None
    a = distinct[0]
    b = next((r for r in distinct[1:] if r["entity"] != a["entity"]), distinct[1])
    return a, b


def _ours_base(run_dir):
    try:
        with open(os.path.join(run_dir, "analysis", "fetch-summary.json")) as f:
            return json.load(f)["ours"][0]["base"]
    except (OSError, ValueError, KeyError, IndexError):
        return None


def run_selftest(run_dir, pairs, platform, log):
    comp = [r for r in pairs if r["status"] == "paired"
            and r.get("ours_path") and r.get("upstream_path")]
    scratch = os.path.join(run_dir, "jobs", "selftest")
    os.makedirs(scratch, exist_ok=True)
    controls, blindspots = [], []

    a, b = _pick_nc1(comp)
    exp1 = "bytediff identical=false AND struct class=structDiff"
    if not a:
        controls.append(_skip("NC1-cross-entity-mispair", exp1,
                              "fewer than 2 distinct comparable payloads"))
    else:
        pa = _copy(os.path.join(run_dir, a["ours_path"]), scratch, "nc1-a")
        pb = _copy(os.path.join(run_dir, b["ours_path"]), scratch, "nc1-b")
        bd = bytediff(pa, pb)
        sd = struct_diff(pa, pb)
        obs = {"identical": bd["identical"], "structClass": sd.get("class")}
        controls.append(_control(
            "NC1-cross-entity-mispair", exp1, obs,
            not bd["identical"] and sd.get("class") == "structDiff",
            cidA=a["cid"], cidB=b["cid"]))

    big = max(comp, key=lambda r: os.path.getsize(
        os.path.join(run_dir, r["ours_path"])), default=None)
    exp2 = "identical=false AND differingBytes=1 AND firstDiff=len//2"
    exp3 = "truncated side classifies loadFail"
    if not big:
        controls.append(_skip("NC2-single-byte-xor", exp2, "no comparable payloads"))
        controls.append(_skip("NC3-truncation", exp3, "no comparable payloads"))
    else:
        src = os.path.join(run_dir, big["ours_path"])
        orig = _copy(src, scratch, "nc23-orig")
        data = open(orig, "rb").read()
        off = len(data) // 2
        mut = bytearray(data)
        mut[off] ^= 0x5A
        mpath = os.path.join(scratch, "nc2-mut")
        with open(mpath, "wb") as f:
            f.write(mut)
        bd = bytediff(orig, mpath)
        obs = {k: bd[k] for k in ("identical", "differingBytes", "firstDiff")}
        controls.append(_control(
            "NC2-single-byte-xor", exp2, obs,
            not bd["identical"] and bd["differingBytes"] == 1
            and bd["firstDiff"] == off,
            offset=off, cid=big["cid"]))
        if len(data) <= 1024:
            controls.append(_skip("NC3-truncation", exp3,
                                  f"payload too small ({len(data)}B)"))
        else:
            tpath = os.path.join(scratch, "nc3-trunc")
            with open(tpath, "wb") as f:
                f.write(data[:-1024])
            sd = struct_diff(orig, tpath)
            controls.append(_control(
                "NC3-truncation", exp3, {"structClass": sd.get("class")},
                sd.get("class") == "loadFailOurs", cid=big["cid"]))

    exp4 = "id_diff cabEqual=false (CAB names are name-derived per platform)"
    other = "mac" if platform == "windows" else "windows"
    controls.append(_nc4(run_dir, comp, platform, other, scratch,
                         exp4, blindspots, log))

    executed = [c for c in controls if not c.get("skipped")]
    all_detected = bool(executed) and all(c["detected"] for c in executed)
    result = {
        "controls": controls,
        "all_detected": all_detected,
        "skipped": [c["name"] for c in controls if c.get("skipped")],
        "blindspots": blindspots,
    }
    for c in controls:
        log(f"selftest: {c['name']} -> "
            + ("SKIPPED (" + c["reason"] + ")" if c.get("skipped")
               else "detected=" + str(c["detected"])))
    return result


def _nc4(run_dir, comp, platform, other, scratch, exp4, blindspots, log):
    if platform in ("linux", "webgl"):
        return _skip("NC4-cross-platform-identity", exp4,
                     f"no cross-platform counterpart convention for {platform}")
    row = next((r for r in comp if r["kind"] != "texture"), None) or \
        next(iter(comp), None)
    if not row:
        return _skip("NC4-cross-platform-identity", exp4, "no comparable payloads")
    base = _ours_base(run_dir)
    if not base:
        return _skip("NC4-cross-platform-identity", exp4,
                     "ours base url unavailable (fetch-summary missing)")
    entity = row["entity"]
    murl = f"{base.rstrip('/')}/manifest/{entity}_{other}.json"
    st, body = http_get(murl, timeout=600, retries=0)
    if st != 200 or not body:
        return _skip("NC4-cross-platform-identity", exp4,
                     f"ours server unreachable for {other} manifest (status={st})")
    man = json.loads(body)
    fname = next((f for f in man.get("files", [])
                  if norm_bundle_name(f, other) == ("bundle", row["cid"])), None)
    if not fname:
        return _skip("NC4-cross-platform-identity", exp4,
                     f"cid {row['cid']} absent from {other} manifest")
    burl = f"{base.rstrip('/')}/{man.get('version', '?')}/{entity}/{fname}"
    st, data = http_get(burl, timeout=600, retries=1)
    if st != 200 or not data:
        return _skip("NC4-cross-platform-identity", exp4,
                     f"{other} payload unfetchable (status={st})")
    opath = os.path.join(scratch, "nc4-" + other)
    with open(opath, "wb") as f:
        f.write(data)
    this = _copy(os.path.join(run_dir, row["ours_path"]), scratch, "nc4-" + platform)
    fail, up_txt, ours_txt = dump_texts(this, opath)
    if fail:
        return _control("NC4-cross-platform-identity", exp4,
                        {"structClass": fail.get("class"),
                         "error": fail.get("error")}, False, cid=row["cid"])
    idd = id_diff(up_txt, ours_txt)
    sd = struct_diff_from_texts(up_txt, ours_txt)
    if sd["class"] == "structIdentical":
        blindspots.append(
            "NC4: cross-platform same-cid pair classifies structIdentical "
            "under vol_norm — platform identity is guaranteed by URL/manifest "
            "pairing, not the normalized structural comparator")
    return _control(
        "NC4-cross-platform-identity", exp4,
        {"cabEqual": idd["cabEqual"], "pidJaccard": idd["pidJaccard"],
         "normStructClass": sd["class"]},
        idd["cabEqual"] is False, cid=row["cid"], otherPlatform=other)
