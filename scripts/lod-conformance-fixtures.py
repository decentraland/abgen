#!/usr/bin/env python3
"""Write the descriptor-conformance fixtures under crate/src/lodgen/testdata/conformance/.

Inputs are the production references fetched by refs/fetch-refs.py:
  <refs>/<scene dir>/{org,zone}/lgu/lods-unity/manifests/<id>-lod-manifest.json
  <refs>/<scene dir>/{org,zone}/lgu/lods-unity/manifests/<id>_InitialSceneState.json
  <refs>/<scene dir>/entity.json                      (content[] = file -> hash)

teapark / amaixen / lounge: manifest + InitialSceneState copied verbatim, content.json = the
entity's file -> hash map. crashworld: a deterministic subset of the 3380-row manifest, i.e. every
row of every entity in the parent chain of (all negative-scale, the first 10 zero-scale, the first
10 w<0) production placements; the InitialSceneState keeps the production entries those entities
produce, ordered the way StaticSceneDescriptorBuilder orders the subset (first-seen gltf src, then
entity insertion order); its content.json keeps only the files the subset's GltfContainer rows name.

The script re-derives the descriptor with the production algorithm (ManifestParser ->
TransformResolver -> StaticSceneDescriptorBuilder) and refuses to write a fixture whose reference
it cannot reproduce, so a refreshed reference that disagrees with the algorithm fails loudly here
instead of in the Rust tests.

Usage: scripts/lod-conformance-fixtures.py --refs DIR --out DIR
"""
import argparse
import json
import math
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from jsonfill import dump_fill as dump  # noqa: E402

SCENES = {
    "teapark": "prod--126_144-tea-park",
    "amaixen": "prod--124_89-mount-amaixen",
    "lounge": "prod-13_55-lounge-audiohotlab",
    "crashworld": "world-crashworld",
}
SUBSET = {"crashworld": "crashworld-subset"}
EXCLUDED_SRCS = {
    "assets/models/out/models/live_events.glb",
    "assets/models/out/models/next_live_events.glb",
}
TOL = 1e-3
ZERO_SCALE_PICK = 10
W_NEG_PICK = 10


def manifests_dir(scene_dir):
    for env in ("org", "zone"):
        d = os.path.join(scene_dir, env, "lgu", "lods-unity", "manifests")
        if os.path.isdir(d):
            return d
    sys.exit(f"no lgu/lods-unity/manifests under {scene_dir}")


def scene_id(mdir):
    """The entity id is the manifest file's prefix (world entity documents carry no `id`)."""
    ids = sorted(f[: -len("-lod-manifest.json")] for f in os.listdir(mdir) if f.endswith("-lod-manifest.json"))
    if len(ids) != 1:
        sys.exit(f"expected exactly one *-lod-manifest.json in {mdir}, found {ids}")
    return ids[0]


def num(v, default):
    return default if v is None else float(v)


def parse(rows):
    """ManifestParser.Parse: last row per (entity, component) wins; MeshRenderer/Material ignored."""
    transforms, gltf, visibility = {}, {}, {}
    for r in rows:
        eid = r["entityId"]
        data = r.get("data")
        if not isinstance(data, dict):
            continue
        name = r.get("componentName")
        if name == "core::Transform":
            p = data.get("position") or {}
            q = data.get("rotation") or {}
            s = data.get("scale") or {}
            transforms[eid] = (
                [num(p.get("x"), 0.0), num(p.get("y"), 0.0), num(p.get("z"), 0.0)],
                [num(q.get("x"), 0.0), num(q.get("y"), 0.0), num(q.get("z"), 0.0), num(q.get("w"), 1.0)],
                [num(s.get("x"), 1.0), num(s.get("y"), 1.0), num(s.get("z"), 1.0)],
                int(data.get("parent") or 0),
            )
        elif name == "core::GltfContainer":
            src = data.get("src")
            if not src or src.lower() in EXCLUDED_SRCS:
                continue
            gltf[eid] = src
        elif name == "core::VisibilityComponent":
            visibility[eid] = bool(data.get("visible", False))
    by_src = {}
    for eid, src in gltf.items():
        by_src.setdefault(src, []).append(eid)
    return transforms, by_src, visibility


def trs(p, q, s):
    x, y, z, w = q
    xx, yy, zz = x * x, y * y, z * z
    xy, xz, yz, wx, wy, wz = x * y, x * z, y * z, w * x, w * y, w * z
    r = [
        [1 - 2 * (yy + zz), 2 * (xy - wz), 2 * (xz + wy)],
        [2 * (xy + wz), 1 - 2 * (xx + zz), 2 * (yz - wx)],
        [2 * (xz - wy), 2 * (yz + wx), 1 - 2 * (xx + yy)],
    ]
    return [[r[i][j] * s[j] for j in range(3)] + [p[i]] for i in range(3)] + [[0.0, 0.0, 0.0, 1.0]]


def mul(a, b):
    return [[sum(a[i][k] * b[k][j] for k in range(4)) for j in range(4)] for i in range(4)]


def world(eid, transforms, visiting=()):
    t = transforms.get(eid)
    if t is None:
        return [[float(i == j) for j in range(4)] for i in range(4)]
    p, q, s, parent = t
    local = trs(p, q, s)
    if parent == 0 or parent == eid or eid in visiting:
        return local
    return mul(world(parent, transforms, visiting + (eid,)), local)


def ancestors(eid, transforms):
    chain, visiting = [eid], set()
    while eid in transforms and eid not in visiting:
        visiting.add(eid)
        parent = transforms[eid][3]
        if parent == 0 or parent == eid:
            break
        chain.append(parent)
        eid = parent
    return chain


def matrix_to_quaternion(r):
    tr = r[0][0] + r[1][1] + r[2][2]
    q = [0.0, 0.0, 0.0, 0.0]
    if tr > 0:
        root = math.sqrt(tr + 1.0)
        q[3] = 0.5 * root
        root = 0.5 / root
        q[0] = (r[2][1] - r[1][2]) * root
        q[1] = (r[0][2] - r[2][0]) * root
        q[2] = (r[1][0] - r[0][1]) * root
    else:
        nxt = [1, 2, 0]
        i = 0
        if r[1][1] > r[0][0]:
            i = 1
        if r[2][2] > r[i][i]:
            i = 2
        j, k = nxt[i], nxt[nxt[i]]
        root = math.sqrt(r[i][i] - r[j][j] - r[k][k] + 1.0)
        q[i] = 0.5 * root
        root = 0.5 / root
        q[3] = (r[k][j] - r[j][k]) * root
        q[j] = (r[j][i] + r[i][j]) * root
        q[k] = (r[k][i] + r[i][k]) * root
    n = math.sqrt(sum(v * v for v in q))
    return [v / n for v in q]


def decompose(m):
    col = lambda c: [m[0][c], m[1][c], m[2][c]]
    c = [col(0), col(1), col(2)]
    scale = [math.sqrt(sum(v * v for v in ci)) for ci in c]
    cross = [c[1][1] * c[2][2] - c[1][2] * c[2][1], c[1][2] * c[2][0] - c[1][0] * c[2][2], c[1][0] * c[2][1] - c[1][1] * c[2][0]]
    det = sum(c[0][i] * cross[i] for i in range(3))
    if det < 0:
        scale[0] = -scale[0]
    try:
        r = [[c[j][i] / scale[j] for j in range(3)] for i in range(3)]
        q = matrix_to_quaternion(r)
    except (ZeroDivisionError, ValueError):
        q = [float("nan")] * 4
    if any(math.isnan(v) for v in q):
        q = [0.0, 0.0, 0.0, 1.0]
    return col(3), q, scale


def build(rows, content):
    """StaticSceneDescriptorBuilder.Build: [(entity, hash, position, rotation, scale)] in production order."""
    transforms, by_src, visibility = parse(rows)
    out = []
    for src, eids in by_src.items():
        h = content.get(src.lower())
        if h is None:
            continue
        for eid in eids:
            if visibility.get(eid) is False:
                continue
            p, q, s = decompose(world(eid, transforms))
            out.append((eid, h, p, q, s))
    return out, transforms


def close(a, b):
    return all(abs(x - y) <= TOL for x, y in zip(a, b))


def same_rotation(a, b):
    return close(a, b) or close(a, [-v for v in b])


def check_reproduces(name, ours, assets):
    if len(ours) != len(assets):
        sys.exit(f"{name}: algorithm gives {len(ours)} placements, reference has {len(assets)}")
    for i, ((eid, h, p, q, s), a) in enumerate(zip(ours, assets)):
        ap, aq, asc = a["position"], a["rotation"], a["scale"]
        if h != a["hash"] or not close(p, [ap["x"], ap["y"], ap["z"]]) or not same_rotation(q, [aq["x"], aq["y"], aq["z"], aq["w"]]) or not close(s, [asc["x"], asc["y"], asc["z"]]):
            sys.exit(f"{name}: placement {i} (entity {eid}) does not reproduce the reference entry")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--refs", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    os.makedirs(args.out, exist_ok=True)
    for name, sub in SCENES.items():
        scene_dir = os.path.join(args.refs, sub)
        with open(os.path.join(scene_dir, "entity.json")) as f:
            entity = json.load(f)
        mdir = manifests_dir(scene_dir)
        sid = scene_id(mdir)
        manifest_path = os.path.join(mdir, f"{sid}-lod-manifest.json")
        iss_path = os.path.join(mdir, f"{sid}_InitialSceneState.json")
        with open(manifest_path) as f:
            rows = json.load(f)
        with open(iss_path) as f:
            iss = json.load(f)
        content_map = {c["file"]: c["hash"] for c in entity["content"]}
        content = {k.lower(): v for k, v in content_map.items()}
        ours, transforms = build(rows, content)
        check_reproduces(name, ours, iss["assets"])
        stem = SUBSET.get(name, name)
        out_manifest = os.path.join(args.out, f"{stem}.lod-manifest.json")
        out_iss = os.path.join(args.out, f"{stem}.InitialSceneState.json")
        if name in SUBSET:
            assets = iss["assets"]
            picked = [i for i, a in enumerate(assets) if a["scale"]["x"] < 0]
            picked += [i for i, a in enumerate(assets) if 0.0 in (a["scale"]["x"], a["scale"]["y"], a["scale"]["z"])][:ZERO_SCALE_PICK]
            picked += [i for i, a in enumerate(assets) if a["rotation"]["w"] < 0][:W_NEG_PICK]
            keep = set()
            for i in picked:
                keep.update(ancestors(ours[i][0], transforms))
            sub_rows = [r for r in rows if r["entityId"] in keep]
            sub_ours, _ = build(sub_rows, content)
            by_entity = {eid: assets[i] for i, (eid, *_rest) in enumerate(ours) if eid in keep}
            sub_assets = [by_entity[eid] for eid, *_rest in sub_ours]
            check_reproduces(f"{name}-subset", sub_ours, sub_assets)
            dump(out_manifest, sub_rows)
            dump(out_iss, {"version": iss["version"], "sceneId": sid, "assets": sub_assets})
            named = {r["data"]["src"].lower() for r in sub_rows if r.get("componentName") == "core::GltfContainer" and isinstance(r.get("data"), dict) and r["data"].get("src")}
            content_map = {k: v for k, v in content_map.items() if k.lower() in named}
            print(f"{name}: subset {len(sub_rows)}/{len(rows)} rows, {len(sub_assets)}/{len(assets)} placements ({len(keep)} entities from {len(picked)} picks)")
        else:
            dump(out_manifest, rows)
            dump(out_iss, iss)
            print(f"{name}: verbatim {len(rows)} rows, {len(iss['assets'])} placements")
        dump(os.path.join(args.out, f"{stem}.content.json"), dict(sorted(content_map.items())))


if __name__ == "__main__":
    main()
