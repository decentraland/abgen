# abgen
Standalone Decentraland asset-bundle converter + ab-cdn-compatible JIT server. The converter is a
clean-room Rust reimplementation of the legacy
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
cargo build --release --examples # bundle dump tools (objdump/texdump/matdump/texcmp/texpng)
scripts/bootstrap-runtime.sh     # integrity-check the vendored runtime data
```
There is also a wasm build: `crate/abgen-wasm/` compiles the converter to wasm32 — see its README.
## Binaries
`target/release/` after `cargo build --release`:

| Bin | What |
|---|---|
| `abgen` | ab-cdn-compatible HTTP server: serves a corpus dir + JIT-converts misses |
| `abgen-build` | single-file local converter CLI (glb -> bundle, `--expect-hash` verify) |
| `abgen-corpus` | batch corpus builder: manifest / `--entity-ids` (add `--fetch-missing` to pull content from a catalyst) / `--world <name>[,...]` (resolve + fetch + convert a world; upstream via `--worlds-url` or `ABGEN_WORLDS_URL`) / `--live-mode` / `--collection-urn` / `--from-reference`; full-corpus runbook in [docs/BATCH.md](docs/BATCH.md) |
| `abgen-verify` | parity differ (ours vs reference bundles, ppm-bits, `--tolerant`) |
| `abgen-lod` | LOD lane CLI: `bundle`, `compare`, `placements`, `assemble`, `atlas`, `simplify`, `generate` |
| `abgen-lambda` | AWS Lambda handler (deployment event in, bundles + manifests to S3; from the `lambda/` workspace member) - see [lambda/README.md](lambda/README.md) |

Plus bundle-inspection examples (`texdump`, `matdump`, `objdump`, `texcmp`, `texpng`) under
`target/release/examples/`.
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
from a connected content DB or proxied from `ABGEN_CATALYST_URL`. The signed/write registry routes
(`/denylist*`, `/queues/*`, `/registry`, `/flush-cache`) **do** mount, but only when
`CONTENT_PG_CONNECTION_STRING` is set in URL form - so a converter deployment that leaves it unset
serves none of them. Setting it exposes admin surface: gate it with `API_ADMIN_TOKEN` and
`DENYLIST_MODERATORS`, and do not put it on a public listener unauthenticated. Route-by-route parity,
header semantics, and the DB-vs-proxy source selection: [docs/ROUTES.md](docs/ROUTES.md).
## Tests
```bash
ABGEN_ROOT="$PWD" cargo test --workspace --lib -- --test-threads=1
```
`--test-threads=1` is required: the lib tests share process-wide `ABGEN_ROOT` state.
## wasm
`crate/abgen-wasm/` compiles the converter lib (default features off) to `wasm32-unknown-unknown` behind
a hand-rolled C ABI; the JS runtime in `crate/abgen-wasm/js/` (worker pool + WebGPU bridge) is what a
host page embeds. It is a plain cargo package with its own committed `Cargo.lock`, excluded from the
workspace: nothing in CI or the release matrix ever needs a wasm toolchain (the pinned one lives in
`crate/abgen-wasm/toolchain/flake.nix`). `crate/abgen-wasm/README.md` documents the build, the headless
driver, the native-vs-wasm byte-parity gate and its decoder contract. Layout caveat: this repo places
`template/` at the repo root, while the wasm lane's helper scripts and the wasm32-only `include_bytes!`
template paths in `crate/src/builder/templates.rs` assume the source layout where it is a sibling of
the crate — an in-repo wasm32 build needs those relative paths bumped one level (`../../template/` ->
`../../../template/`). No workspace target compiles for wasm32, so the divergence never touches CI.
## Features
One compile-time feature flag: `server` (on by default) gates the abcdn HTTP server + registry stack
(axum/tokio/sqlx and the `dcl-contents` crate); the `abgen` bin has `required-features = ["server"]`.
Library consumers - `lambda/`, `crate/abgen-native/`, `crate/abgen-wasm/` - build with
`default-features = false` and get the converter without the server stack. Under the default
features every capability below builds into every native binary; `wasm32` builds target-gate the
server, content-DB, and GPU code off automatically. Each capability is activated at runtime, not at
build time.

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

On NixOS: `nix develop`, or `nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#gcc nixpkgs#gnumake
nixpkgs#cmake nixpkgs#pkg-config`. No openssl (ureq uses rustls), no protobuf. Optional: libturbojpeg
(dlopen'd, or set `TURBOJPEG_LIB=/path`); without it JPEG decode falls back to the vendored libjpeg9c
path - valid output but not byte-parity with production; startup logs `turbojpeg_available`.
## Platform matrix
| Platform | Build |
|---|---|
| Linux x86_64 | native (`nix develop` or bare toolchain) |
| macOS (Apple Silicon) | `nix develop` (clang stdenv) |
| Windows | WSL2, checkout on the WSL filesystem |
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
| `ABGEN_HTTP_HOST` / `ABGEN_HTTP_PORT` | `127.0.0.1` / `5147` | bind address (legacy aliases `HTTP_SERVER_HOST` / `HTTP_SERVER_PORT` still honored) |
| `ABGEN_OUT_ROOT` | `./data/ab-generator/out` | corpus root served + JIT write-back target (may start empty) |
| `ABGEN_CATALYST_URL` | `http://127.0.0.1:5141/content` | content server (standalone: `https://peer.decentraland.org/content`) |
| `ABGEN_CONTENT_DISK` | unset | optional local content-store root (disk-first client) |
| `ABGEN_CACHE_DIR` | `./abgen-serve-cache` | JIT conversion cache |
| `ABGEN_VERSION` | `v49` | advertised AB version |
| `ABGEN_MANIFEST_CONTENT_SERVER_URL` | `https://peer.decentraland.org/content` | `contentServerUrl` stamped into manifests |
| `ABGEN_ROOT` | repo root | root containing `template/` |
| `ABGEN_METRICS_BEARER_TOKEN` | unset | if set, `/metrics` requires this bearer token |
| `ABGEN_JIT_FAIL_TTL_S` | `60` | failure negative-cache TTL for entity JIT builds; concurrent identical requests single-flight per `{entity}:{platform}` |
| `ABGEN_JIT_CONTENT_DIGEST` | `0` | freshness for content servers whose declared hashes are not content-addressed (the `@dcl/sdk-commands` preview server keys them by file path). Defaults to ON when `ABGEN_CATALYST_URL` points at a loopback host (a dev preview server), OFF otherwise; explicit values always win. When on, manifest requests re-download the entity's convertible content, sha256 it, and prune stale conversions when bytes changed under an unchanged hash. Changed non-glb files (textures/`.bin`) also invalidate every glb bundle of the entity. Debounced per entity via `ABGEN_REVALIDATE_DEBOUNCE_S` (default `2`) |
| `ABGEN_UPSTREAM_AB_CDN` | unset | read-through to a production ab-cdn (e.g. `https://ab-cdn.decentraland.org`): manifest/bundle/LOD/ISS requests that 404 through every local and JIT lane are streamed from upstream without persisting anything locally. Lets a client point its whole optimized-assets base URL here (wearables/emotes keep working) while only local scene entities are built locally. Paths containing `b64-` preview ids are never proxied |
| `RUST_LOG` | `abgen=info,tower_http=info` | tracing filter |
| `CONTENT_PG_CONNECTION_STRING` (or `POSTGRES_*` parts) | unset | content DB; URL form also mounts the signed registry routes |
### S3 space cache (read-through + write-back)
Enabled when `ABGEN_S3_BUCKET` is non-empty (or `ABGEN_USE_SPACE=1`):
- credentials, first match wins: `ABGEN_S3_ACCESS_KEY`/`ABGEN_S3_SECRET_KEY`, `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`, or the ECS container role (`AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` or `_FULL_URI`)
- session token: `ABGEN_S3_SESSION_TOKEN` or `AWS_SESSION_TOKEN`
- endpoint `ABGEN_S3_ENDPOINT`; region `ABGEN_S3_REGION`|`AWS_REGION`; `ABGEN_S3_PATH_STYLE` for path-style addressing
- `ABGEN_S3_READ_ONLY=1` - read-through only; server write-back is refused
- `ABGEN_FALLBACK_VERSION` (default `v41`) - extra version prefix tried on space-cache lookups
- `ABGEN_WORLDS_CONTENT_URL` - worlds-content-server fallback for entity/content fetches that miss the primary source (default public worlds server; `0`/`off`/empty disables)

Uploads carry the production consumer-server's object metadata, derived from the key by
`space::object_headers`: bundles `application/wasm` + `public,max-age=31536000,immutable`
(cdn-uploader's comma-joined spelling), scene sources (`.js`/`.json`/`.crdt`) the direct-upload
spelling `public, max-age=31536000, immutable`, manifests (`manifest/…`, `lods-unity/manifests/…`)
`application/json` + `private, max-age=0, no-cache`, `.br` keys
`public,no-transform,max-age=31536000,immutable`. No `Content-Encoding` is set on `.br` objects.

### Asset-reuse mode (upstream converter parity)
ON by default, matching the ab-cdn deployment's asset-reuse naming from v49 onward: scene
glb/gltf bundles use the upstream converter's canonical naming and shared bucket layout (applies
to the JIT server and `abgen-corpus`). Set `ABGEN_DEPS_DIGEST=0` to fall back to legacy
`{hash}_{platform}` names and entity-scoped space keys — needed only for parity runs against
pre-v49 reference trees (e.g. `--live-mode` sampling of v15–v41 vintages, whose manifests list
non-digest names).
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
preset. The `image` job in [`.github/workflows/release.yml`](.github/workflows/release.yml) builds
that image and pushes it to `ghcr.io/decentraland/abgen:<tag>` (and `:latest`) on every `v*` tag.
The workspace also ships an AWS Lambda consumer in `lambda/` (the `abgen-lambda` bin);
`nix build .#lambdaImage` produces its container image, which the `lambda-image` job pushes to ECR
on the same tags - see [lambda/README.md](lambda/README.md). The org
services-pipeline that publishes the same service to quay (service-name `abgen`) is configured
externally in the private `decentraland/definitions` repo, not in this tree. Cutting a release -
tagging, what the pipeline does, and what to check afterwards: [DEVELOPMENT.md](DEVELOPMENT.md).

Parity posture and clean-room provenance are documented separately: [PARITY.md](PARITY.md) and
[PROVENANCE.md](PROVENANCE.md).
## Provenance
This repository is a generated standalone export: the server crate maps to `crate/` with a small
overlay; the content component + unsigned registry surface ship as the shared `crate/dcl-contents`
crate, copied verbatim from the internal tree. Mechanical differences from the internal source:
signed/denylist/queue registry routes deleted; the server bin is `abgen` and the local build CLI is
`abgen-build`.
## License
**Apache-2.0** - full text in [LICENSE](LICENSE). Contributions are accepted under the same
terms. Vendored third-party licenses and
shader/template bundle provenance: [PROVENANCE.md](PROVENANCE.md). Note: the lib sets
`#[global_allocator] mimalloc`; any downstream embedding the lib inherits it.
