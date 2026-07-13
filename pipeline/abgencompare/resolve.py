"""Entity resolution: pointer (x,y | urn) or entity id -> entity-meta rows.

Content-server API:
  POST {content}/entities/active {"pointers":[...]}  -> [entity]
  GET  {content}/contents/{entityId}                 -> the entity JSON file
Kind classification (verbatim from the campaign's build-pairs.py):
  glb/gltf ext -> `animated` when the entity type is emote, else `glb`;
  image exts   -> `texture`; anything else -> other(exts).
"""
import json

from .util import http_get, http_post_json

IMAGE_EXTS = {"png", "jpg", "jpeg", "gif", "psd", "tga", "bmp", "tif", "tiff"}


def looks_like_entity_id(p):
    return (p.startswith("Qm") and len(p) == 46) or p.startswith(("bafk", "bafy", "bafr"))


def resolve_pointer(content_url, pointer, log):
    """-> list of raw entity dicts (usually one)."""
    content_url = content_url.rstrip("/")
    if looks_like_entity_id(pointer):
        url = f"{content_url}/contents/{pointer}"
        status, body = http_get(url, timeout=60)
        if status != 200 or not body:
            raise SystemExit(f"entity {pointer} not fetchable: {url} -> {status}")
        ent = json.loads(body)
        ent.setdefault("id", pointer)
        return [ent]
    url = f"{content_url}/entities/active"
    status, body = http_post_json(url, {"pointers": [pointer]}, timeout=60)
    if status != 200 or not body:
        raise SystemExit(f"pointer resolve failed: POST {url} -> {status}")
    ents = json.loads(body)
    if not ents:
        raise SystemExit(f"pointer {pointer!r} resolves to no active entity")
    log(f"resolve: pointer {pointer!r} -> {[e.get('id') for e in ents]}")
    return ents


def entity_meta_row(ent):
    """Raw entity JSON -> entity-meta row (content as path->cid map)."""
    content = {}
    for c in ent.get("content") or []:
        path = c.get("file") or c.get("key") or ""
        content[path] = c.get("hash") or ""
    return {
        "entity": ent.get("id"),
        "type": ent.get("type") or "unknown",
        "pointers": ent.get("pointers") or [],
        "content": content,
        "metadata_keys": sorted((ent.get("metadata") or {}).keys()),
    }


def cid_kind_map(meta_row):
    """lowercase cid -> kind for one entity (campaign kind_of, per-entity)."""
    etype = meta_row.get("type")
    by_cid = {}
    for path, cid in (meta_row.get("content") or {}).items():
        ext = path.rsplit(".", 1)[-1].lower() if "." in path else ""
        by_cid.setdefault(cid.lower(), set()).add(ext)
    out = {}
    for cid, exts in by_cid.items():
        if exts & {"glb", "gltf"}:
            out[cid] = "animated" if etype == "emote" else "glb"
        elif exts & IMAGE_EXTS:
            out[cid] = "texture"
        else:
            out[cid] = "other(" + ",".join(sorted(exts)) + ")"
    return out
