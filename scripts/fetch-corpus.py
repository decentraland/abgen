#!/usr/bin/env python3
"""Build a local abgen content store with a representative conversion corpus.

Layout matches LocalContentStore: <root>/<sha1(cid)[:2 bytes]hex>/<cid>
Writes <root>/ids-{scenes,wearables,emotes,all}.txt for abgen-bench --entity-ids.
"""
import concurrent.futures as cf
import hashlib
import json
import os
import sys
import urllib.request

CATALYST = "https://peer.decentraland.org/content"
NFT_API = "https://marketplace-api.decentraland.org/v1/items"
ROOT = os.path.expanduser(sys.argv[1] if len(sys.argv) > 1 else "~/corpus")

SCENE_POINTERS = ["0,0"]
SCATTER = ["-9,-9", "20,20", "-29,55", "46,-67", "-75,-75", "61,10", "-114,34",
           "13,-137", "100,100", "-50,0", "0,75", "-140,-140"]
N_WEARABLES = 4
N_EMOTES = 3

def http_json(url, body=None):
    req = urllib.request.Request(url, headers={"User-Agent": "abgen-bench-corpus"})
    if body is not None:
        req.data = json.dumps(body).encode()
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.load(r)

def store_path(cid):
    prefix = hashlib.sha1(cid.encode()).hexdigest()[:4]
    d = os.path.join(ROOT, prefix)
    os.makedirs(d, exist_ok=True)
    return os.path.join(d, cid)

def store_bytes(cid, data):
    p = store_path(cid)
    tmp = p + ".tmp"
    with open(tmp, "wb") as f:
        f.write(data)
    os.replace(tmp, p)

def fetch_content(cid):
    p = store_path(cid)
    if os.path.exists(p) and os.path.getsize(p) > 0:
        return 0
    url = f"{CATALYST}/contents/{cid}"
    req = urllib.request.Request(url, headers={"User-Agent": "abgen-bench-corpus"})
    with urllib.request.urlopen(req, timeout=300) as r:
        data = r.read()
    store_bytes(cid, data)
    return len(data)

def active_entities(pointers):
    return http_json(f"{CATALYST}/entities/active", {"pointers": pointers})

def entity_size(ent):
    return sum(c.get("size") or 0 for c in ent["content"]) or len(ent["content"])

def main():
    os.makedirs(ROOT, exist_ok=True)
    groups = {"scenes": [], "wearables": [], "emotes": []}

    genesis = active_entities(SCENE_POINTERS)
    assert genesis, "no scene at 0,0?"
    scenes = {e["id"]: e for e in genesis}
    scatter = active_entities(SCATTER)
    extra = sorted((e for e in scatter if e["id"] not in scenes),
                   key=entity_size, reverse=True)
    picked = []
    if extra:
        picked.append(extra[0])
        if len(extra) > 1:
            picked.append(extra[-1])
    for e in picked:
        scenes[e["id"]] = e
    groups["scenes"] = list(scenes.values())

    for cat, n, key in (("wearable", N_WEARABLES, "wearables"),
                        ("emote", N_EMOTES, "emotes")):
        items = http_json(f"{NFT_API}?first={n * 3}&category={cat}&sortBy=newest")
        urns = []
        for it in items.get("data", []):
            urn = it.get("urn") or it.get("id")
            if urn and urn not in urns:
                urns.append(urn)
            if len(urns) >= n:
                break
        ents = active_entities(urns) if urns else []
        groups[key] = ents[:n]

    hashes = set()
    for key, ents in groups.items():
        ids = []
        for e in ents:
            store_bytes(e["id"], json.dumps(e).encode())
            ids.append(e["id"])
            for c in e["content"]:
                hashes.add(c["hash"])
        with open(os.path.join(ROOT, f"ids-{key}.txt"), "w") as f:
            f.write("\n".join(ids) + "\n")
        print(f"{key}: {len(ids)} entities", flush=True)

    with open(os.path.join(ROOT, "ids-all.txt"), "w") as f:
        for key in ("wearables", "emotes", "scenes"):
            for e in groups[key]:
                f.write(e["id"] + "\n")

    print(f"downloading {len(hashes)} content files...", flush=True)
    total = 0
    failed = []
    with cf.ThreadPoolExecutor(max_workers=8) as ex:
        futs = {ex.submit(fetch_content, h): h for h in hashes}
        for i, fut in enumerate(cf.as_completed(futs)):
            try:
                total += fut.result()
            except Exception as exc:
                failed.append((futs[fut], str(exc)))
            if (i + 1) % 50 == 0:
                print(f"  {i + 1}/{len(hashes)} ({total / 1e6:.0f} MB new)", flush=True)
    print(f"done: {len(hashes) - len(failed)}/{len(hashes)} files, "
          f"{total / 1e6:.0f} MB downloaded, root={ROOT}", flush=True)
    for h, err in failed[:10]:
        print(f"FAILED {h}: {err}", flush=True)
    sys.exit(1 if failed else 0)

if __name__ == "__main__":
    main()
