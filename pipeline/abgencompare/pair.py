"""Pairing: ours <-> upstream bundle files, per entity.

Match key = lowercase(fileCID) (upstream lowercases Qm CIDs in manifests/on
disk); name forms handled by fetch.norm_bundle_name. Row schema mirrors the
campaign's pairs-<plat>.jsonl minus absolute paths (run-relative here).
"""
from .resolve import cid_kind_map


def build_pairs(run_id, platform, meta_rows, ours_summaries, upstream_summaries, log):
    """-> (pairs_rows, unfillable_rows). One paired row per cid present on
    either side; pair ids <platform initial><NNNNN> in deterministic order."""
    kinds = {m["entity"]: cid_kind_map(m) for m in meta_rows}
    rows, unfillable = [], []
    n = 0
    for ours in ours_summaries:
        entity = ours["entity"]
        up = next((u for u in upstream_summaries if u["entity"] == entity), None)
        omap = {v["cid"]: (f, v) for f, v in ours["bundles"].items()}
        umap = {v["cid"]: (f, v) for f, v in ((up or {}).get("bundles") or {}).items()}
        kmap = kinds.get(entity, {})
        for cid in sorted(set(omap) | set(umap)):
            kind = kmap.get(cid, "unknown")
            in_o, in_u = cid in omap, cid in umap
            if in_o and in_u:
                n += 1
                pid = f"{platform[0]}{n:05d}"
                ofn, ov = omap[cid]
                ufn, uv = umap[cid]
                purged = uv["path"] is None
                row = {
                    "status": "paired",
                    "pair_id": pid,
                    "run": run_id,
                    "entity": entity,
                    "platform": platform,
                    "cid": cid,
                    "kind": kind,
                    "ours_name": ofn,
                    "ours_path": ov["path"],
                    "ours_source": "jit-live local abgen server",
                    "upstream_name": ufn,
                    "upstream_path": uv["path"],
                    "upstream_local": not purged,
                    "upstream_payload_purged": purged,
                    "upstream_url": f"{up['base'].rstrip('/')}/{up['version']}/{entity}/{ufn}",
                    "manifest_version": up.get("version"),
                    "ours_version": ours.get("version"),
                }
                if ov["path"] is None:
                    row["status"] = "ours-unfetchable"
                rows.append(row)
            elif in_o:
                ofn, ov = omap[cid]
                rows.append({
                    "status": "unpairable",
                    "reason": "ours_only_no_upstream_counterpart",
                    "run": run_id, "entity": entity, "platform": platform,
                    "cid": cid, "kind": kind,
                    "ours_name": ofn, "ours_path": ov["path"],
                    "ours_source": "jit-live local abgen server",
                    "manifest_version": (up or {}).get("version"),
                })
                unfillable.append({
                    "pair": None, "cid": cid, "entity": entity,
                    "why": "upstream manifest lacks this bundle cid",
                })
            else:
                ufn, uv = umap[cid]
                rows.append({
                    "status": "unpairable",
                    "reason": "upstream_only_ours_lacks_bundle",
                    "run": run_id, "entity": entity, "platform": platform,
                    "cid": cid, "kind": kind,
                    "upstream_name": ufn, "upstream_path": uv["path"],
                    "manifest_version": (up or {}).get("version"),
                })
                unfillable.append({
                    "pair": None, "cid": cid, "entity": entity,
                    "why": "bundle cid absent from local abgen manifest",
                })
    paired = [r for r in rows if r["status"] == "paired"]
    log(
        f"pair: {len(paired)} paired / "
        f"{sum(1 for r in rows if r['status'] == 'unpairable')} unpairable / "
        f"{sum(1 for r in paired if r['upstream_payload_purged'])} upstream-purged"
    )
    return rows, unfillable
