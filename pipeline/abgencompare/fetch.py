"""Bundle-side fetching: local abgen server ("ours") and upstream ab-cdn.

Manifest URL shapes (verified against asset-bundle-mirror / freshness-probe):
  windows/mac/linux: {base}/manifest/<entity>_<platform>.json
  webgl:             {base}/manifest/<entity>.json
Bundle payloads:     {base}/{manifest.version}/{entity}/{fileName}

Both sides are fetched over HTTP and archived verbatim into the run dir
(ours/... and upstream/...), so the run is self-contained even when the
spawned server's scratch out_root is later discarded.
"""
import json
import os
import re
import time

from .util import http_get

HEX32 = re.compile(r"^[0-9a-f]{32}$")
SKIP_ARTIFACTS = {"dcl", "buildlogtep.json"}


def manifest_url(base, entity, platform):
    suffix = "" if platform == "webgl" else f"_{platform}"
    return f"{base.rstrip('/')}/manifest/{entity}{suffix}.json"


def norm_bundle_name(fname, platform):
    """-> ('bundle', cid_lower) | ('artifact', None) | ('unparsed', None).

    Verbatim port of build-pairs.py norm_upstream: handles <cid>_<plat>
    (v12–v48), <cid>_<32hex>_<plat> (v49 build-id infix) and bare <cid>
    (vintage webgl); ignores dcl/json/manifest/entity_*/scene_ignore_*.
    """
    if (
        fname in SKIP_ARTIFACTS
        or fname.endswith(".json")
        or fname.endswith(".manifest")
        or fname.startswith("entity_")
        or fname.startswith("scene_ignore")
    ):
        return ("artifact", None)
    parts = fname.split("_")
    if parts[-1] == platform:
        parts = parts[:-1]
    if len(parts) == 2 and HEX32.match(parts[1]):
        parts = parts[:1]
    if len(parts) != 1 or not parts[0]:
        return ("unparsed", None)
    return ("bundle", parts[0].lower())


def stage_upstream_shader(run_dir, summaries, base, platform, log):
    """Entity-shipped shader bundle (harness/README.md §1.3): scan the fetched
    upstream manifests and download the platform's ``DCL/Scene`` shader into
    ``<run>/shader/``. Two historical manifest shapes (both live-verified
    2026-07-04):

    - **dcl-dir era** (v38 through at least v44): ``files`` lists a literal
      ``dcl`` entry; the payloads live UNDER it at
      ``{base}/{ver}/{entity}/dcl/scene_ignore_<platform>`` — real
      platform-specific builds whose CAB names match ``cabname.rs``
      (mac = CAB-5ba4993b…, windows = CAB-51fbd4c9…);
    - **flat era** (≤v36): ``files`` lists ``scene_ignore_windows`` even on
      mac manifests (one shared build; byte-identical across platforms, old
      CAB convention).

    Returns the absolute staged path, or None (v49-era manifests list
    neither). CAB matching is what makes renders resolve materials — a
    wrong-platform donor loads fine but renders magenta on both sides."""
    for s in summaries:
        ver = s.get("version")
        if not ver:
            continue
        files = s.get("files", [])
        candidates = []
        if "dcl" in files:
            candidates.append(f"dcl/scene_ignore_{platform}")
        candidates.extend(f for f in files if f.startswith("scene_ignore"))
        for fname in candidates:
            url = f"{base.rstrip('/')}/{ver}/{s['entity']}/{fname}"
            st, data = http_get(url, timeout=120, retries=1)
            if st == 200 and data:
                dest = os.path.join(run_dir, "shader", os.path.basename(fname))
                os.makedirs(os.path.dirname(dest), exist_ok=True)
                with open(dest, "wb") as f:
                    f.write(data)
                log(f"shader: staged entity-shipped {fname} "
                    f"({len(data)} bytes) from {url}")
                return dest
            log(f"shader: {fname} candidate in {s['entity']} manifest "
                f"unfetchable (status={st})")
    return None


def fetch_side(base, side, entity, platform, run_dir, log, sleep=0.0, timeout=120,
               manifest_timeout=900, manifest_name=None):
    """Fetch manifest + every bundle payload for one side into the run dir.

    side: 'ours' | 'upstream'. Returns a summary dict (also written to
    manifests/<entity>.<side>.json next to the raw manifest; watch mode passes
    manifest_name to keep per-platform manifests apart in its rolling run).
    """
    murl = manifest_url(base, entity, platform)
    log(f"{side}: GET {murl}")
    status, body = http_get(murl, timeout=manifest_timeout, retries=1)
    summary = {
        "side": side,
        "base": base,
        "entity": entity,
        "platform": platform,
        "manifest_url": murl,
        "manifest_status": status,
        "version": None,
        "files": [],
        "bundles": {},
        "purged": [],
        "unparsed": [],
    }
    mpath = os.path.join(run_dir, "manifests",
                         manifest_name or f"{entity}.{side}.json")
    if status != 200 or not body:
        log(f"{side}: manifest unavailable ({status})")
        with open(mpath, "w") as f:
            json.dump({"error": f"manifest fetch status={status}"}, f)
        return summary
    with open(mpath, "wb") as f:
        f.write(body)
    man = json.loads(body)
    ver = man.get("version", "?")
    summary["version"] = ver
    summary["exitCode"] = man.get("exitCode")
    summary["date"] = man.get("date")
    summary["files"] = man.get("files", [])

    dest_root = os.path.join(run_dir, side, entity, platform)
    os.makedirs(dest_root, exist_ok=True)
    for fname in summary["files"]:
        kind, cid = norm_bundle_name(fname, platform)
        if kind == "artifact":
            continue
        if kind == "unparsed":
            summary["unparsed"].append(fname)
            continue
        burl = f"{base.rstrip('/')}/{ver}/{entity}/{fname}"
        if sleep:
            time.sleep(sleep)
        st, data = http_get(burl, timeout=timeout, retries=2)
        rel = os.path.join(side, entity, platform, fname)
        if st == 200 and data:
            with open(os.path.join(run_dir, rel), "wb") as f:
                f.write(data)
            summary["bundles"][fname] = {
                "status": st, "size": len(data), "path": rel, "cid": cid,
            }
            log(f"{side}: {fname} {len(data)} bytes")
        else:
            summary["bundles"][fname] = {
                "status": st, "size": 0, "path": None, "cid": cid,
            }
            summary["purged"].append(fname)
            log(f"{side}: {fname} UNFETCHABLE (status={st})")
    return summary
