# Full-corpus batch conversion

The runbook for converting every active Decentraland entity (or any subset) into an
ab-cdn-shaped serving tree with `abgen-corpus`. `--cdn-layout` writes the exact layout the
`abgen` server serves, so a finished run drops straight into `ABGEN_OUT_ROOT`.

## 1. Enumerate entity ids

Any newline-separated entity-id file works (blank lines and `#` comments ignored). Public sources,
no database required:

- **Catalyst snapshots** - the full active set. `GET
  https://peer.decentraland.org/content/snapshots` lists time-ranged snapshot files; download
  each `hash` from `https://peer.decentraland.org/content/contents/{hash}`. A snapshot is a
  header line followed by one JSON record per active entity (`entityId`, `entityType`,
  `pointers`, timestamp); union across snapshots, keep the newest entity per pointer, extract
  the `entityId`s for the types you want.
- **Targeted sets** - `POST /entities/active` on any catalyst content server with
  `{"pointers": [...]}` (parcels or wearable/emote URNs) returns the active entity per pointer.
- **Worlds** - enumerated by name, not id: `GET
  https://worlds-content-server.decentraland.org/index` lists every world with its scene entity
  ids, or skip ids entirely and pass names to `abgen-corpus --world`, which resolves and fetches
  by itself.

For scale: the 2026-07-10 full-corpus run resolved 74,292 active entities (25,569 scenes +
46,254 wearables + 662 emotes + 1,807 world scenes).

## 2. The local content store

Batch modes derive bundles from a local sharded content store (`--content-dir`, default
`./content`, env twin `ABGEN_CONTENT_ROOT`): every file (entity json and content payloads
alike) lives at `<root>/<sha1(cid)[:4]>/<cid>` - the shard is the first four hex chars of the
CID's SHA-1.

## 3. Fetching content: `--fetch-missing`

Without it, `--entity-ids` and `--collection-urn` derive from the store only - ids whose entity
json or content files are absent are counted and skipped (the count prints at the end). With
`--fetch-missing`:

- `--entity-ids`: each listed entity's json + content files are downloaded into the store from
  the catalyst at `--content-server-url` (default `https://peer.decentraland.org/content`)
  before deriving - the same fetch path `--world` uses. Already-present files are kept;
  per-entity fetch failures warn and fall through to the missing-count skip.
- `--collection-urn`: missing content is downloaded from the URLs already present in the
  lambdas response.

`--world` always fetches; `--fetch-missing` with neither `--entity-ids` nor `--collection-urn`
is a usage error.

## 4. The invocation

```bash
abgen-corpus --entity-ids ids.txt out_root \
  --cdn-layout --fetch-missing --skip-existing \
  --platform windows,mac -j 32 [--gpu]
```

- `--cdn-layout` writes the serving shape: `<entity>/<platform>.manifest.json` plus bundle
  binaries under `<entity>/<platform>/<hash>_<platform>`, shared binaries hardlinked across
  entities, reconcile pass at the end. Client texture mode (`--real-textures --v38-compat`) is
  the default under `--cdn-layout`.
- `--platform windows,mac` (either order) is the fused pair pass: each entity derived and each
  bundle parsed + encoded once, then serialized and compressed per platform. Other platform
  combinations are rejected - linux/webgl change the encode itself.
- `--skip-existing` makes re-runs incremental top-offs. Default rebuilds every bundle
  (golden/determinism workflows rely on that); `--force` states that explicitly.
- `-j N` sets parallel jobs (default: CPU count).

## 5. GPU vs CPU

Both GPU backends compile into every build with no feature flags — CUDA (`libcuda` is dlopen'd at
runtime from the NVIDIA driver, no CUDA toolkit needed to build, PTX kernels vendored) and the
portable `wgpu` Vulkan/Metal/DX12 backend. Opt in per run with `--gpu` (server: `ABGEN_GPU=1`);
pick the backend with `ABGEN_GPU_BACKEND=auto|cuda|wgpu|off`. Self-qualification gate and exit-2
contract: [README's Features table](../README.md#features). Without `ABGEN_GPU` (or a qualifying
device) the CPU path produces the same corpus, just slower; texture encode dominates batch cost, so
GPU is the recommendation for full-corpus runs.

## 6. Disk budget

The full two-platform corpus out_root measured 215 GB (2026-07-10 run, hardlink-deduped,
585,712 bundle files per platform). The content store is additional and scales with how much
content you mirror. Plan for both plus scratch headroom.

## 7. Exit semantics and the known failure class

A run prints `DONE built=N skipped=N errs=N total=N` and exits 1 whenever `errs > 0` (manifest
and reconcile errors included) - a full-corpus run that converts everything convertible still
exits 1. Known baseline: ~260 failures per platform (259 mac / 258 windows on the 2026-07-10
full run), almost all the legacy `load gltf inputs` class - old entities whose glTF payloads
don't parse. Treat that count as the expected floor, alert on growth, top off with
`--skip-existing` re-runs.

An alternative is warming through a running `abgen` server (its registry POSTs eager-build
misses), but direct batch is the efficient route for a full corpus.

## Performance

Measured GPU numbers (CUDA backend, one NVIDIA RTX PRO 6000 Blackwell workstation GPU, warm
local content store):

- Full corpus, 74,292 active entities x windows+mac (585,712 bundle files per platform):
  27.0 min total wall at `-j 32` as two phased single-platform passes (mac 781 s, windows
  841 s; ~74% of outputs built fresh, the rest skipped as already present from an earlier
  partial run).
- After the fused-pass work landed, one fused `--platform windows,mac` cold pass over the same
  id set measured 764 s (~12.7 min) wall; on an 8,054-id cold sample the fused pass measured
  2.54x faster than the phased platform pair.

No full-corpus CPU wall time has been measured; no CPU projections are published. Conversion is
fast in practice: the live browser wasm demo converts an 88 kB wearable GLB to a validated UnityFS
bundle in ~0.3 s (0.8 s cold with module load) and a 3.1 MB scene NPC GLB in ~1.2 s including the
LOD1 bake, single-threaded CPU wasm - the native binary (SIMD/GPU, multithreaded) is faster still.
The wasm lab ships in this repo (`crate/wasm-poc/` + `site/wasm/`).
