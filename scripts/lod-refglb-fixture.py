#!/usr/bin/env python3
"""Write a `<tag>.refglb.json` handedness fixture from a production `lods-unity/lods/<id>_1.glb`.

The fixture pins the world-space AABB of the reference GLB (gltfpack output: KHR_mesh_quantization
u16 positions dequantized through the mesh node's translation/scale) together with everything the
number was derived from: node chain, accessor min/max, parcel rect, and the source sha256 as
recorded in the refs manifest. `crate/tests/lod_handedness.rs` re-derives the AABB from these
fields and checks the ISS placements against it.

    scripts/lod-refglb-fixture.py --refs <refs root holding manifest.json> \
        --scene prod--126_144-tea-park --tag teapark --out crate/src/lodgen/testdata/handedness

`manifest.json` is a list of scene entries `{name, pointer, entityId, artifacts: [...]}`; each
artifact is `{kind, url, path, sha256, bytes}` and the kinds read here are `_1.glb`, `ISS` and
`entity`. Artifact paths are resolved as written (absolute, or relative to the cwd); the fixture
records them relative to `--refs`.

Only the standard library is used.
"""

import argparse
import hashlib
import json
import math
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from jsonfill import dump_fill  # noqa: E402

GLB_MAGIC = b"glTF"
CHUNK_JSON = 0x4E4F534A


def sha256_of(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def glb_json(path):
    with open(path, "rb") as f:
        data = f.read()
    if data[:4] != GLB_MAGIC:
        sys.exit(f"{path}: not a GLB")
    length, ctype = struct.unpack_from("<II", data, 12)
    if ctype != CHUNK_JSON:
        sys.exit(f"{path}: first chunk is not JSON")
    return json.loads(data[20:20 + length]), len(data)


def quat_to_mat3(q):
    x, y, z, w = q
    return [
        [1 - 2 * (y * y + z * z), 2 * (x * y - z * w), 2 * (x * z + y * w)],
        [2 * (x * y + z * w), 1 - 2 * (x * x + z * z), 2 * (y * z - x * w)],
        [2 * (x * z - y * w), 2 * (y * z + x * w), 1 - 2 * (x * x + y * y)],
    ]


def mat4_from_trs(t, r, s):
    m3 = quat_to_mat3(r)
    m = [[0.0] * 4 for _ in range(4)]
    for row in range(3):
        for col in range(3):
            m[row][col] = m3[row][col] * s[col]
        m[row][3] = t[row]
    m[3][3] = 1.0
    return m


def mat4_from_gltf(node):
    if "matrix" in node:
        c = node["matrix"]
        return [[c[col * 4 + row] for col in range(4)] for row in range(4)]
    return mat4_from_trs(
        node.get("translation", [0.0, 0.0, 0.0]),
        node.get("rotation", [0.0, 0.0, 0.0, 1.0]),
        node.get("scale", [1.0, 1.0, 1.0]),
    )


def mat4_mul(a, b):
    return [[sum(a[i][k] * b[k][j] for k in range(4)) for j in range(4)] for i in range(4)]


def mul_point(m, p):
    return [m[i][0] * p[0] + m[i][1] * p[1] + m[i][2] * p[2] + m[i][3] for i in range(3)]


def is_identity(m):
    for i in range(4):
        for j in range(4):
            if abs(m[i][j] - (1.0 if i == j else 0.0)) > 0.0:
                return False
    return True


def has_rotation(node):
    return "matrix" in node or node.get("rotation", [0.0, 0.0, 0.0, 1.0]) != [0.0, 0.0, 0.0, 1.0]


def walk(gltf, idx, parent_world, chain, out):
    node = gltf["nodes"][idx]
    world = mat4_mul(parent_world, mat4_from_gltf(node))
    name = node.get("name")
    if "mesh" in node:
        out.append((idx, node, world, list(chain)))
    for child in node.get("children", []):
        walk(gltf, child, world, chain + [name if name is not None else f"#{idx}"], out)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--refs", required=True, help="reference set root holding manifest.json")
    ap.add_argument("--scene", required=True, help="scene `name` in manifest.json")
    ap.add_argument("--tag", required=True, help="fixture basename (teapark, amaixen)")
    ap.add_argument("--out", required=True, help="output directory for <tag>.refglb.json")
    args = ap.parse_args()

    with open(os.path.join(args.refs, "manifest.json")) as f:
        manifest = json.load(f)
    entry = next((e for e in manifest if e["name"] == args.scene), None)
    if entry is None:
        sys.exit(f"scene {args.scene!r} not in {args.refs}/manifest.json")
    by_kind = {a["kind"]: a for a in entry["artifacts"]}
    for kind in ("_1.glb", "ISS", "entity"):
        if kind not in by_kind:
            sys.exit(f"{args.scene}: manifest has no {kind!r} artifact")
    glb_art, iss_art, ent_art = by_kind["_1.glb"], by_kind["ISS"], by_kind["entity"]

    for art in (glb_art, iss_art):
        got = sha256_of(art["path"])
        if got != art["sha256"]:
            sys.exit(f"{art['path']}: sha256 {got} != manifest {art['sha256']}")

    with open(ent_art["path"]) as f:
        ent = json.load(f)
    ent = ent[0] if isinstance(ent, list) else ent
    scene_meta = ent["metadata"]["scene"]
    base = [int(v) for v in scene_meta["base"].split(",")]
    parcels = [[int(v) for v in p.split(",")] for p in scene_meta["parcels"]]
    bx, bz = base[0] * 16.0, base[1] * 16.0
    min_px = min(p[0] for p in parcels) * 16.0
    max_px = (max(p[0] for p in parcels) + 1) * 16.0
    min_pz = min(p[1] for p in parcels) * 16.0
    max_pz = (max(p[1] for p in parcels) + 1) * 16.0
    parcel_rect_rh = {"x": [bx - max_px, bx - min_px], "z": [min_pz - bz, max_pz - bz]}

    gltf, glb_bytes = glb_json(glb_art["path"])
    if glb_bytes != glb_art["bytes"]:
        sys.exit(f"{glb_art['path']}: {glb_bytes} bytes != manifest {glb_art['bytes']}")
    scene_idx = gltf.get("scene", 0)
    roots = gltf["scenes"][scene_idx]["nodes"]
    identity = [[1.0 if i == j else 0.0 for j in range(4)] for i in range(4)]
    mesh_nodes = []
    for r in roots:
        walk(gltf, r, identity, [], mesh_nodes)
    if not mesh_nodes:
        sys.exit("reference GLB has no mesh nodes reachable from the scene")

    nodes_out, prims_out = [], []
    aabb_min = [math.inf] * 3
    aabb_max = [-math.inf] * 3
    chain_has_rotation = False
    for idx, node, world, chain in mesh_nodes:
        ancestors = [gltf["nodes"][i] for i in range(len(gltf["nodes"])) if idx in gltf["nodes"][i].get("children", [])]
        chain_rot = has_rotation(node) or any(has_rotation(a) for a in ancestors)
        chain_has_rotation = chain_has_rotation or chain_rot
        nodes_out.append({
            "index": idx,
            "name": node.get("name"),
            "mesh": node["mesh"],
            "chain": chain,
            "translation": node.get("translation", [0.0, 0.0, 0.0]),
            "rotation": node.get("rotation", [0.0, 0.0, 0.0, 1.0]),
            "scale": node.get("scale", [1.0, 1.0, 1.0]),
            "ancestorsIdentity": all(is_identity(mat4_from_gltf(a)) for a in ancestors),
        })
        mesh = gltf["meshes"][node["mesh"]]
        for pi, prim in enumerate(mesh["primitives"]):
            acc = gltf["accessors"][prim["attributes"]["POSITION"]]
            lo, hi = acc["min"], acc["max"]
            corners = [[lo[0] if (c & 1) == 0 else hi[0], lo[1] if (c & 2) == 0 else hi[1], lo[2] if (c & 4) == 0 else hi[2]] for c in range(8)]
            ws = [mul_point(world, c) for c in corners]
            wmin = [min(w[i] for w in ws) for i in range(3)]
            wmax = [max(w[i] for w in ws) for i in range(3)]
            for i in range(3):
                aabb_min[i] = min(aabb_min[i], wmin[i])
                aabb_max[i] = max(aabb_max[i], wmax[i])
            mat = gltf["materials"][prim["material"]] if "material" in prim else None
            prims_out.append({
                "node": idx,
                "primitive": pi,
                "material": mat.get("name") if mat else None,
                "alphaMode": (mat or {}).get("alphaMode", "OPAQUE"),
                "positionAccessor": prim["attributes"]["POSITION"],
                "componentType": acc["componentType"],
                "normalized": bool(acc.get("normalized", False)),
                "count": acc["count"],
                "min": lo,
                "max": hi,
                "worldMin": wmin,
                "worldMax": wmax,
            })

    with open(iss_art["path"]) as f:
        iss = json.load(f)

    fixture = {
        "tag": args.tag,
        "refsName": entry["name"],
        "pointer": entry["pointer"],
        "sceneId": entry["entityId"],
        "base": base,
        "parcels": parcels,
        "parcelRectRh": parcel_rect_rh,
        "source": {
            "kind": glb_art["kind"],
            "url": glb_art["url"],
            "pathInRefs": os.path.relpath(glb_art["path"], args.refs),
            "sha256": glb_art["sha256"],
            "bytes": glb_art["bytes"],
            "generator": gltf.get("asset", {}).get("generator"),
            "extensionsRequired": gltf.get("extensionsRequired", []),
            "sceneName": gltf["scenes"][scene_idx].get("name"),
        },
        "iss": {
            "url": iss_art["url"],
            "sha256": iss_art["sha256"],
            "assets": len(iss.get("assets", [])),
        },
        "formula": (
            "world = W * q for every corner q of the POSITION accessor [min, max] box, W = product of the "
            "node TRS matrices from the scene root down to the mesh node in glTF (right-handed) space; "
            "with no rotation in the chain this is translation + scale * q per axis"
        ),
        "chainHasRotation": chain_has_rotation,
        "nodes": nodes_out,
        "primitives": prims_out,
        "aabb": {"min": aabb_min, "max": aabb_max},
        "generatedBy": "scripts/lod-refglb-fixture.py",
    }

    os.makedirs(args.out, exist_ok=True)
    out_json = os.path.join(args.out, f"{args.tag}.refglb.json")
    dump_fill(out_json, fixture)
    print(f"{out_json}: aabb min={aabb_min} max={aabb_max} prims={len(prims_out)} parcelRectRh={parcel_rect_rh}")
    print(f"iss: {fixture['iss']['assets']} assets sha256={iss_art['sha256']}")


if __name__ == "__main__":
    main()
