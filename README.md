# abgen
Standalone Decentraland asset-bundle converter + ab-cdn-compatible JIT server, plus **abgen-compare**,
a parity pipeline that measures output against the production CDN in a browser. The converter is a
clean-room Rust reimplementation of the Unity
[asset-bundle-converter](https://github.com/decentraland/asset-bundle-converter): GLB/GLTF content from
any catalyst content server in, Unity AssetBundles layout-compatible with the production
`ab-cdn.decentraland.org` CDN out (byte-level parity posture in [PARITY.md](PARITY.md)). See
[Provenance](#provenance).
## Quick start
Prebuilt binaries ship two ways: release archives on the tagged GitHub releases, and npm -
`npx @dcl/abgen` runs the server directly (linux x64/arm64, windows x64/arm64, macOS x64/arm64; the platform
binary installs as an `optionalDependency` of `@dcl/abgen`, and `require('@dcl/abgen').binPath()`
resolves it for embedding tools - see [npm/abgen](npm/abgen/README.md)). From source:
```bash
git clone <this-repo> abgen && cd abgen
cargo build --release            # no postgres, no openssl, no protobuf
cargo build --release --examples # bundle dump tools the compare site shells out to
scripts/bootstrap-runtime.sh     # integrity-check the vendored runtime data
./pipeline/abgen-compare serve   # http://localhost:5197 - setup wizard, then results
./pipeline/abgen-compare run --pointer '100,100' --platform windows
```
`run` accepts a scene parcel, an entity id, or a wearable/emote URN. It is headless-first: with nothing
but this repo it fetches the upstream bundles, generates ours, pairs them, and produces
manifest/structural/texture-decode verdicts. Pixel-level verdicts (rendered screenshots, diff heatmaps)
are an optional stage that drives a Unity Editor and takes two inputs, stored once:
```bash
./pipeline/abgen-compare config set unity_editor  /path/to/6000.x/Unity
./pipeline/abgen-compare config set unity_project /path/to/unity-explorer/Explorer
```
The client-faithful host project is a
[decentraland/unity-explorer](https://github.com/decentraland/unity-explorer) checkout with
`harness/unity-explorer-abgen.patch` applied; `harness/project-template/` is the self-contained
fallback. Verdict labels and thresholds are specified by `pipeline/abgencompare/classify.py` (the
classifier is the spec). Setup details: [pipeline/README.md](pipeline/README.md) and
[harness/README.md](harness/README.md).
## Binaries
`target/release/` after `cargo build --release`:

| Bin | What |
|---|---|
| `abgen` | ab-cdn-compatible HTTP server: serves a corpus dir + JIT-converts misses (feature `server`, on by default) |
| `abgen-build` | single-file local converter CLI (glb -> bundle, `--expect-hash` verify) |
| `abgen-corpus` | batch corpus builder: manifest / `--entity-ids` (add `--fetch-missing` to pull content from a catalyst) / `--world <name>[,...]` (resolve + fetch + convert a world; upstream via `--worlds-url` or `ABGEN_WORLDS_URL`) / `--live-mode` / `--collection-urn` / `--from-reference`; full-corpus runbook in [docs/BATCH.md](docs/BATCH.md) |
| `abgen-verify` | parity differ (ours vs reference bundles, ppm-bits, `--tolerant`) |
| `abgen-lod` | LOD lane CLI: `bundle`, `compare`, `placements`, `assemble`, `atlas`, `simplify`, `generate` |

Plus `pipeline/abgen-compare` (Python >= 3.9, stdlib-only, run in place) and bundle-inspection examples
(`texdump`, `matdump`, `objdump`, `crndump`, `texcmp`, `texpng`) under `target/release/examples/`.
`scripts/lod-parity.sh` runs the same baked LOD GLB through the Unity converter (a sibling
[asset-bundle-converter](https://github.com/decentraland/asset-bundle-converter) checkout + editor) and
through `abgen-lod bundle`, then diffs the two bundles (`abgen-lod compare` + `matdump`); `--site`
publishes the verdicts to the compare site's `/lod.html`.
## Server
```bash
ABGEN_CATALYST_URL=https://peer.decentraland.org/content ./target/release/abgen
curl -s localhost:5147/health | jq .   # expect template_ok:true, templates_missing:[]
```
Served routes - the full asset-bundle delivery surface an explorer needs:
- `GET /manifest/{entity}_{platform}.json` - bundle manifest, JIT-converting on a miss
- `GET /{version}/{cid}/{file}` - versioned bundle blobs (JIT on miss)
- `GET /LOD/{level}/{file}` - classic per-scene LODs
- `GET /lods-unity/manifests/*` - ISS LOD descriptors
- `POST /entities/active`, `POST /entities/versions` - the asset-bundle registry index routes the explorer's registry client calls to resolve bundle versions
- `GET /health`, `/readyz`, `/livez`, `/metrics`, `/ping`

An explorer can point both its asset-bundle CDN base and its registry base at this one host (what the
`optimized-assets` client flag targets). The unsigned registry surface from the in-tree
[`dcl-contents`](crate/dcl-contents) crate is always mounted (`POST /profiles`,
`POST /profiles/metadata`, `GET /entities/status/{id}`, `GET /worlds/{world_name}/manifest`), sourced
from a connected content DB (feature `content-db`) or proxied from `ABGEN_CATALYST_URL`. The
signed/write registry extras (denylist, queues, admin, `flush-cache`) are NOT served - they belong to a
catalyst or registry service, not this converter. Route-by-route parity, header semantics, and the
DB-vs-proxy source selection: [docs/ROUTES.md](docs/ROUTES.md).
## Tests
```bash
ABGEN_ROOT="$PWD" cargo test --workspace --lib -- --test-threads=1
```
`--test-threads=1` is required: the lib tests share process-wide `ABGEN_ROOT` state.
## wasm lab
`crate/abgen-wasm/` compiles the converter lib (default features off) to `wasm32-unknown-unknown` behind
a hand-rolled C ABI and drives it from the static pages in `site/wasm/` - drop a glb/gltf/zip, get real
UnityFS bundles in the browser. It is a plain cargo package with its own committed `Cargo.lock`,
excluded from the workspace: nothing in CI or the release matrix ever needs a wasm toolchain (the
pinned one lives in `crate/abgen-wasm/toolchain/flake.nix`). `crate/abgen-wasm/README.md` documents the
build, the headless driver, the native-vs-wasm byte-parity gate and its decoder contract. Layout
caveat: this repo places `site/` and `template/` at the repo root, while the wasm lane's helper scripts
and the wasm32-only `include_bytes!` template paths in `crate/src/builder/templates.rs` assume the
source layout where both are siblings of the crate — an in-repo wasm32 build needs those relative paths
bumped one level (`../../template/` -> `../../../template/`, and the `../site` copy in
`crate/abgen-wasm/build.sh` adjusted likewise). No workspace target compiles for wasm32, so the
divergence never touches CI. Self-hosting the lab: `site/` is fully static - any file server works,
but it must serve `.wasm` with the `application/wasm` MIME type or `WebAssembly.instantiateStreaming`
falls over.
## Live JIT compare
`abgen-compare watch` compares every entity a running JIT server converts against the upstream ab-cdn
(bytes / structure / texture decode, no renders) into one rolling run the compare site auto-refreshes,
with backfill of pre-existing conversions and a crash-safe resume cursor. Every flag has an env twin
for service deployment; `./pipeline/abgen-compare watch --help` documents them.
## Features
There are no compile-time feature flags. Every capability below builds into every native binary;
`wasm32` builds target-gate the server, content-DB, and GPU code off automatically. Each capability
is activated at runtime, not at build time.

| Capability | Built | Enable at runtime |
|---|---|---|
| tokio/axum HTTP JIT server (`abcdn` module + `abgen` bin) | always (native targets) | run the `abgen` bin |
| catalyst content-DB index (sqlx/postgres, via the in-tree `dcl-contents` crate) for real timestamps + deployer on `/entities/*` and the unsigned registry routes (`/profiles*`, `/entities/status/{id}`, `/worlds/{name}/manifest`) | always | set a content-DB connection (`CONTENT_PG_CONNECTION_STRING` or `POSTGRES_CONTENT_*`); without one the built-in content-client fallback serves the index routes with `timestamp: 0`, empty deployer, and the registry routes proxy the upstream catalyst |
| CUDA GPU BC7/BC5 encode path (`libcuda` dlopen'd at runtime, no toolkit needed to build; PTX kernels vendored in `crate/src/gpu/kernel.ptx`, regen from `crate/kernel-ptx/`) | always | opt in per run with `--gpu` / `ABGEN_GPU=1`, backend pick via `ABGEN_GPU_BACKEND=auto\|cuda\|wgpu\|off` |
| portable `wgpu` compute backend (Vulkan/Metal/DX12), same dispatch + qualification gate | always | `ABGEN_GPU_BACKEND=wgpu` (or `auto`, which tries CUDA first) |

GPU backends self-qualify per device at enable time: the selected backend (`auto` tries CUDA, then
wgpu) must reproduce the CPU BC7 encoder bit-for-bit on a probe matrix (sizes x srgb x perceptual x
profile, full mip chains) or it is disqualified and `--gpu`/`ABGEN_GPU=1` exits 2 instead of silently
degrading - a qualified GPU run produces byte-identical output to the CPU path. `ABGEN_GPU_QUALIFY=0`
skips the probe.

The slim, async-free converter lib + CLIs are the `wasm32-unknown-unknown` target build (the server,
content-DB, and GPU stacks are `cfg(not(target_arch = "wasm32"))`).
## Toolchain
- rustc/cargo (edition 2021; the vendored `draco_decoder` is edition 2024)
- cc + a C++ toolchain (vendored crunch, libjpeg9c)
- cmake + make (`draco_decoder` builds upstream draco C++; missing cmake is the #1 build failure)
- Python >= 3.9 for `abgen-compare` (stdlib-only; `numpy` + `Pillow` are optional accelerators)

On NixOS: `nix develop`, or `nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#gcc nixpkgs#gnumake
nixpkgs#cmake nixpkgs#pkg-config`. No openssl (ureq uses rustls), no protobuf. Optional: libturbojpeg
(dlopen'd, or set `TURBOJPEG_LIB=/path`); without it JPEG decode falls back to the vendored libjpeg9c
path - valid output but not byte-parity with production; startup logs `turbojpeg_available`.
## Platform matrix
| Platform | Build | Unity renders |
|---|---|---|
| Linux x86_64 | native (`nix develop` or bare toolchain) | native Unity editor; primary target |
| macOS (Apple Silicon) | `nix develop` (clang stdenv) | native Unity editor (Metal); verified end-to-end |
| Windows | WSL2, checkout on the WSL filesystem | Windows-native Unity through the wslpath bridge (`pipeline/abgencompare/wsl.py`); verified end-to-end |

WSL2: `--unity` accepts `C:\...` or `/mnt/c/...`; paths and `AB_*` env cross the boundary
automatically; render inputs are staged to `--win-staging` (default `/mnt/c/abgen-runs/<run-id>`); run
renders from an interactive WSL shell so Unity gets a real GPU session.
## Runtime data
Vendored in-repo and sha256-pinned; a fresh clone is zero-config. `scripts/bootstrap-runtime.sh`
verifies the sha256 of both sets and, when a payload is missing, prints the git-history commands to
restore it - it never fetches or re-downloads:
- `template/*.windows.bundle` - 4 typetree-donor bundles read by `builder.rs::load_template()`
- `crate/shader/scene_ignore_{windows,mac}` - the compiled per-platform `DCL/Scene` shader bundles (the canonical ab-cdn URLs 404; the vendored copies are the source of truth, provenance in [PROVENANCE.md](PROVENANCE.md)). The server self-primes them on first request - no bucket pre-seeding step; the `lit_ignore_*`/texarray/linux shader names 404 by design (absent upstream too)
## Environment variables
### Server (`crate/src/abcdn/config.rs`)
| Var | Default | Meaning |
|---|---|---|
| `HTTP_SERVER_HOST` / `HTTP_SERVER_PORT` | `127.0.0.1` / `5147` | bind address |
| `ABGEN_OUT_ROOT` | `./data/ab-generator/out` | corpus root served + JIT write-back target (may start empty) |
| `ABGEN_CATALYST_URL` | `http://127.0.0.1:5141/content` | content server (standalone: `https://peer.decentraland.org/content`) |
| `ABGEN_CONTENT_DISK` | unset | optional local content-store root (disk-first client) |
| `ABGEN_CACHE_DIR` | `./abgen-serve-cache` | JIT conversion cache |
| `ABGEN_VERSION` | `v41` | advertised AB version |
| `ABGEN_MANIFEST_CONTENT_SERVER_URL` | `https://peer.decentraland.org/content` | `contentServerUrl` stamped into manifests |
| `ABGEN_ROOT` | repo root | root containing `template/` |
| `ABGEN_METRICS_BEARER_TOKEN` | unset | if set, `/metrics` requires this bearer token |
| `ABGEN_JIT_FAIL_TTL_S` | `60` | failure negative-cache TTL for entity JIT builds; concurrent identical requests single-flight per `{entity}:{platform}` |
| `ABGEN_JIT_CONTENT_DIGEST` | `0` | freshness for content servers whose declared hashes are not content-addressed (the `@dcl/sdk-commands` preview server keys them by file path). Defaults to ON when `ABGEN_CATALYST_URL` points at a loopback host (a dev preview server), OFF otherwise; explicit values always win. When on, manifest requests re-download the entity's convertible content, sha256 it, and prune stale conversions when bytes changed under an unchanged hash. Changed non-glb files (textures/`.bin`) also invalidate every glb bundle of the entity. Debounced per entity via `ABGEN_REVALIDATE_DEBOUNCE_S` (default `2`) |
| `ABGEN_UPSTREAM_AB_CDN` | unset | read-through to a production ab-cdn (e.g. `https://ab-cdn.decentraland.org`): manifest/bundle/LOD/ISS requests that 404 through every local and JIT lane are streamed from upstream without persisting anything locally. Lets a client point its whole optimized-assets base URL here (wearables/emotes keep working) while only local scene entities are built locally. Paths containing `b64-` preview ids are never proxied |
| `RUST_LOG` | `abgen=info,tower_http=info` | tracing filter |
| `CONTENT_PG_CONNECTION_STRING` (or `POSTGRES_*` parts) | unset | feature `content-db` only |
### S3 space cache (read-through + write-back)
Enabled when `ABGEN_S3_BUCKET` is non-empty (or `ABGEN_USE_SPACE=1`):
- credentials, first match wins: `ABGEN_S3_ACCESS_KEY`/`ABGEN_S3_SECRET_KEY`, `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`, or the ECS container role (`AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` or `_FULL_URI`)
- session token: `ABGEN_S3_SESSION_TOKEN` or `AWS_SESSION_TOKEN`
- endpoint `ABGEN_S3_ENDPOINT`; region `ABGEN_S3_REGION`|`AWS_REGION`; `ABGEN_S3_PATH_STYLE` for path-style addressing
- `ABGEN_S3_READ_ONLY=1` - read-through only; server write-back is refused
- `ABGEN_FALLBACK_VERSION` (default `v41`) - extra version prefix tried on space-cache lookups
- `ABGEN_WORLDS_CONTENT_URL` - worlds-content-server fallback for entity/content fetches that miss the primary source (default public worlds server; `0`/`off`/empty disables)

### Asset-reuse mode (upstream converter parity)
ON by default — the ab-cdn deployment has run asset-reuse since v49. Scene glb/gltf bundles use
the upstream converter's canonical naming and shared bucket layout (applies to the JIT server
and `abgen-corpus`). Set `ABGEN_DEPS_DIGEST=0` to fall back to legacy `{hash}_{platform}` names
and entity-scoped space keys — needed only for parity runs against pre-v49 reference trees
(e.g. `--live-mode` sampling of v15–v41 vintages, whose manifests list non-digest names).
- glb/gltf bundles are named `{hash}_{depsdigest}_{platform}` — the digest is a 128-bit hash of
  the glb's resolved `(file, hash)` dependency pairs (`.bin` + textures), so a glb whose
  dependency set changes lands at a new name/key; textures stay `{hash}_{platform}`
- space keys move from entity-scoped `{version}/{entity}/{bundle}` to the shared
  `{version}/assets/{bundle}` layout (entity-scoped keys remain a read fallback)
- before building, the space is HEAD-probed at the canonical key; hits are listed in the entity
  manifest without rebuilding, so bundles are converted once across entities
- a glb whose deps can't be resolved is skipped (upstream skipped-assets semantics) unless
  `ABGEN_MAGENTA_MISSING` is on, in which case unresolvable deps are dropped from the digest and
  the build substitutes placeholder textures

### Registry index eager build
On `POST /entities/active|versions`, entities missing a converted bundle are queued for conversion; the
request waits up to the deadline, builds finish in the background. Knobs: `ABGEN_INDEX_EAGER_BUILD`
(default on; `0`/`false`/`no` disables), `ABGEN_INDEX_BUILD_PLATFORMS` (default `windows,mac`),
`ABGEN_INDEX_BUILD_CONCURRENCY` (default: CPU count), `ABGEN_INDEX_BUILD_DEADLINE_MS` (default
`20000`), `ABGEN_INDEX_BUILD_MAX_QUEUE` (default `0` = unlimited; when exceeded, new builds are skipped).
### LOD JIT lane
`ABGEN_LOD_JIT=1` enables JIT LOD builds on `GET /LOD/{0|1}/...` misses; needs `gltfpack`
(`ABGEN_GLTFPACK` or `$PATH`), fails closed without it. Knobs: `ABGEN_LOD_MANIFEST_BUILDER` (unset
limits JIT to scenes with a published ISS descriptor), `ABGEN_LOD_CACHE_DIR` (default:
`ABGEN_CACHE_DIR`), `ABGEN_LOD_JIT_TIMEOUT_S` / `ABGEN_LOD_JIT_FAIL_TTL_S` (defaults `600` / `3600`),
`ABGEN_LOD_BUILD_CONCURRENCY` (default `1`). Builds stage in a per-build workdir; only gate-passed
output is promoted into the serving root, so rejected bundles are never servable.
### Conversion / parity knobs
- `ABGEN_SHADER_BUNDLE` - path to `scene_ignore_windows` (default `crate/shader/scene_ignore_windows`; siblings like `scene_ignore_mac` resolve from the same dir)
- `ABGEN_CONTENT_ROOT` - local sharded content store root (default `./content`)
- compat: `ABGEN_V38_COMPAT`, `ABGEN_V38_TIMESTAMP`, `ABGEN_COLLECTION_MODE`, `ABGEN_REAL_TEXTURES`, `ABGEN_MAGENTA_MISSING`, `ABGEN_JPEG_TURBO_BOX`, `ABGEN_JPEG_GLB_9C` (`live.rs::Proxy::new` sets some process-wide; not thread-safe before the first build)
- debug/dev: `ABGEN_BC7_CACHE`, `ABGEN_BC7_SCALAR`, `ABGEN_BC7_NO512`, `ABGEN_BC7_CAPTURE`, `ABGEN_LZ4_DUMP`, `ABGEN_TEST_CRN_OURS`/`_REF`/`_DUMP`
## Deployment
The container image is built straight from the pinned Nix flake - `nix build .#dockerImage`
(`dockerTools.buildLayeredImage`, `tini` init, no base OS) - producing an image that runs `abgen` as an
unprivileged user on `:5147` with `template/` and `shader/` baked in and `ABGEN_ROOT`/`ABGEN_OUT_ROOT`
preset. The [`.github/workflows/image.yml`](.github/workflows/image.yml) workflow builds that image and
pushes it to `ghcr.io/decentraland/abgen:<tag>` (and `:latest`) on every `v*` tag. The org
services-pipeline that publishes the same service to quay (service-name `abgen`) is configured
externally in the private `decentraland/definitions` repo, not in this tree.

Parity posture and clean-room provenance are documented separately: [PARITY.md](PARITY.md) and
[PROVENANCE.md](PROVENANCE.md).
## Provenance
This repository is a generated standalone export: the server crate maps to `crate/` with a small
overlay; the pipeline/site/harness trees ship verbatim; the content component + unsigned registry
surface ship as the shared `crate/dcl-contents` crate (feature `content-db`), copied verbatim from
the internal tree. Mechanical differences from the internal source: signed/denylist/queue registry
routes deleted; the server bin is `abgen` and the local build CLI is `abgen-build`.
## License
AGPL-3.0-or-later (the Rust crate) - full text in [LICENSE](LICENSE). Vendored third-party licenses and
shader/template bundle provenance: [PROVENANCE.md](PROVENANCE.md). Note: the lib sets
`#[global_allocator] mimalloc`; any downstream embedding the lib inherits it.
