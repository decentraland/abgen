# Manifest-builder golden baselines

Captured 2026-07-17 by running the npm scene-lod-entities-manifest-builder
through `abgen-lod placements --iss off` (the pre-swap run_manifest_builder
path, commit 7740117 — the npm fallback has since been replaced by the
embedded scene runtime) with a node shell wrapped around the repo flake:

```
env ABGEN_LOD_SUBPROC_TIMEOUT_S=900 \
  nix shell nixpkgs#nodejs --command nix develop -c \
  cargo run --bin abgen-lod -- placements --coords <C> --iss off \
    --manifest-builder <checkout of decentraland/scene-lod-entities-manifest-builder> \
    --workdir /tmp/abgen-lod-golden
```

Tool pin: @dcl/ecs 7.24.5 (package-lock of the builder checkout). Catalyst
base: https://peer.decentraland.org (the builder's committed .env; abgen passes
only --coords and --overwrite). The coords resolved to:

| coords | entity | lane | stderr |
|---|---|---|---|
| -150,150 | bafkreiau2nk2wuki5tw42runje2grjqza7tcfvzabc554jqsxc42ax3nqm | SDK7 | source: manifest-builder (0 placements, 0 mesh-renderer-only skipped, 0 unresolved src) |
| 100,100 | QmVDhg6mQyBBnyk36N6YWHH8dbLYM8kpUaH2VxmwZKFj6T | SDK6 (adaption layer) | source: manifest-builder (1 placements, 0 mesh-renderer-only skipped, 0 unresolved src) |

A redeploy at either coordinate changes the resolved entity and invalidates the
golden; re-resolve with `POST /content/entities/active {"pointers":["<C>"]}`
and compare against the ids above before trusting a mismatch.

## Files

- `golden_*_.placements.json` — stdout of `abgen-lod placements` (sorted
  Placement array, byte-stable across runs).
- `golden_*_.manifest.json` — the raw `<entityId>-lod-manifest.json` the npm
  tool wrote in output-manifests/, for parse_lod_manifest_full cross-checks and
  the parity oracle.
- `getstate_*.bin` — the component payload bytes (not the PUT frames) of the
  synthetic crdtGetState initial state the tool hands every scene: default
  Transform for entities 1 and 2 (identical 44-byte blob, committed once),
  UiCanvasInformation (componentId 1054) on entity 0, CameraMode (componentId
  1072) on entity 2. The last two are genuinely empty: @dcl/ecs 7.24.5
  serializes the all-default protobuf messages to zero bytes, so the synthetic
  PUTs carry dataLen=0. Scene-independent constants of the pinned tool.

## Lane coverage

The SDK7 scene at -150,150 is a SoundR audio scene: it emits only Transform,
AudioSource (1020) and AvatarAttach (1073) PUTs for entities 512-515, so its
manifest holds four default Transforms and zero GltfContainer rows and the
placements golden is `[]`. Its five GLB content files are wearable
REPRESENTATION models the scene never places. It anchors the empty-placements
lane and the synthetic-initial-state plumbing; the SDK6 scene at 100,100
anchors the GltfContainer lane (one placement, models/SCENE.glb, resolved
hash, non-trivial rotation and scale).
