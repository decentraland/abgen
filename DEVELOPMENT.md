# abgen - development guide

Service overview, deploy flow, and the runtime env contract: [README.md](README.md).

Standalone Decentraland asset-bundle converter + ab-cdn-compatible JIT server, plus **abgen-compare**,
a parity pipeline that measures output against the production CDN in a browser. The converter is a
clean-room Rust reimplementation of the Unity
[asset-bundle-converter](https://github.com/decentraland/asset-bundle-converter): GLB/GLTF content from
any catalyst content server in, Unity AssetBundles byte-compatible with the production
`ab-cdn.decentraland.org` layout out.
## Quick start
Prebuilt binaries ship two ways: release archives on the tagged GitHub releases, and npm -
`npx @dcl/abgen` runs the server directly (linux x64/arm64, windows x64/arm64, macOS x64/arm64; the platform
binary installs as an `optionalDependency` of `@dcl/abgen`, and `require('@dcl/abgen').binPath()`
resolves it for embedding tools - see [npm/abgen](npm/abgen/README.md)). The npm channel is
distribution-only: nothing in abgen builds or runs with node. From source:
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
| `abgen-corpus` | batch corpus builder: manifest / `--entity-ids` / `--world <name>[,...]` (resolve + fetch + convert a world; upstream via `--worlds-url` or `ABGEN_WORLDS_URL`) / `--live` / `--collection` / `--from-ref` |
| `abgen-verify` | parity differ (ours vs reference bundles, ppm-bits, `--tolerant`) |
| `abgen-lod` | LOD lane CLI: `bundle`, `compare`, `placements`, `parse-manifest`, `assemble`, `atlas`, `simplify`, `generate` |
| `abgen-space` | S3 space cache tool: `status`, `get`, `put`, `push`, `pull` (`--json` emits one machine-readable stats line) |

Plus `pipeline/abgen-compare` (Python >= 3.9, stdlib-only, run in place) and bundle-inspection examples
(`texdump`, `matdump`, `objdump`, `crndump`, `texcmp`, `texpng`) under `target/release/examples/`.
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
`optimized-assets` client flag targets). Registry extras are NOT served: `/profiles*`,
`/entities/status*`, `/worlds/{name}/manifest`, `/queues/*`, `/denylist*`, and the registry's
admin / write / `flush-cache` routes all 404 - they belong to a catalyst or registry service, not this
converter.
## Tests
```bash
ABGEN_ROOT="$PWD" cargo test --workspace --lib -- --test-threads=1
```
`--test-threads=1` is required: the lib tests share process-wide `ABGEN_ROOT` state.

Scene-runtime goldens (network: resolve two pinned scenes on `peer.decentraland.org` and byte-compare
the embedded runtime's placements against the committed captures):
```bash
ABGEN_ROOT="$PWD" cargo test --workspace --lib -- --test-threads=1 --ignored golden_
```
`scripts/lod-parity-oracle.sh <manifest-builder-checkout> <X,Y> [...]` runs the npm
scene-lod-entities-manifest-builder next to the embedded runtime and diffs placements per scene -
manual and operator-local, never CI.
## Live JIT compare
`abgen-compare watch` compares every entity a running JIT server converts against the upstream ab-cdn
(bytes / structure / texture decode, no renders) into one rolling run the compare site auto-refreshes,
with backfill of pre-existing conversions and a crash-safe resume cursor. Every flag has an env twin
for service deployment; `./pipeline/abgen-compare watch --help` documents them.
## Features
| Feature | Default | Adds |
|---|---|---|
| `server` | yes | tokio/axum HTTP server (`abcdn` module + `abgen` bin) |
| `scene-runtime` | yes | embedded QuickJS SDK7/SDK6 scene runtime for LOD placements (quickjs-ng via `rquickjs`, C built by cc) |
| `engine-v8` | no | alternative V8 backend for the scene runtime; exists for the cross-engine parity test |
| `content-db` | no | catalyst content-DB index (sqlx/postgres) for real timestamps + deployer on `/entities/*`; without it the built-in content-client fallback serves the same routes with `timestamp: 0`, empty deployer |

`cargo build --no-default-features` builds the pure converter lib + CLIs with no async stack. With
`scene-runtime` off, the LOD placements fallback is limited to scenes with a published ISS descriptor.
## Toolchain
- rustc/cargo (edition 2021; the vendored `draco_decoder` is edition 2024)
- cc + a C++ toolchain (vendored crunch, libjpeg9c; `rquickjs-sys` builds quickjs-ng the same way)
- cmake + make (`draco_decoder` builds upstream draco C++; missing cmake is the #1 build failure)
- Python >= 3.9 for `abgen-compare` (stdlib-only; `numpy` + `Pillow` are optional accelerators)

No node/npm anywhere: building and running every abgen feature - converter, server, lodgen including
the LOD placements fallback - requires neither. The npm manifest-builder's behavior is embedded as a
QuickJS scene runtime (feature `scene-runtime`, default on). node appears in exactly one place: the
tag-gated CI job that publishes the prebuilt binaries to npm ([npm/abgen](npm/abgen/README.md)).

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
verifies both sets and re-fetches if missing:
- `template/*.windows.bundle` - 4 typetree-donor bundles read by `builder.rs::load_template()`
- `crate/shader/scene_ignore_windows` - the compiled `DCL/Scene` shader bundle (the old ab-cdn URL 404s; the vendored copy is the source of truth)
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
| `RUST_LOG` | `abgen=info,tower_http=info` | tracing filter |
| `CONTENT_PG_CONNECTION_STRING` (or `POSTGRES_*` parts) | unset | feature `content-db` only |
### S3 space cache (read-through + write-back)
Enabled when `ABGEN_S3_BUCKET` is non-empty (or `ABGEN_USE_SPACE=1`):
- credentials, first match wins: `ABGEN_S3_ACCESS_KEY`/`ABGEN_S3_SECRET_KEY`, `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`, or the ECS container role (`AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` or `_FULL_URI`)
- session token: `ABGEN_S3_SESSION_TOKEN` or `AWS_SESSION_TOKEN`
- endpoint `ABGEN_S3_ENDPOINT`; region `ABGEN_S3_REGION`|`AWS_REGION`; `ABGEN_S3_PATH_STYLE` for path-style addressing
- `ABGEN_S3_READ_ONLY=1` - read-through only; server write-back and `abgen-space put`/`push` are refused
- `ABGEN_FALLBACK_VERSION` (default `v41`) - extra version prefix tried on space-cache lookups
- `ABGEN_WORLDS_CONTENT_URL` - worlds-content-server fallback for entity/content fetches that miss the primary source (default public worlds server; `0`/`off`/empty disables)

`abgen-space status` prints the resolved config and probes auth.
### Registry index eager build
On `POST /entities/active|versions`, entities missing a converted bundle are queued for conversion; the
request waits up to the deadline, builds finish in the background. Knobs: `ABGEN_INDEX_EAGER_BUILD`
(default on; `0`/`false`/`no`/`off` disables), `ABGEN_INDEX_BUILD_PLATFORMS` (default `windows,mac`),
`ABGEN_INDEX_BUILD_CONCURRENCY` (default: CPU count), `ABGEN_INDEX_BUILD_DEADLINE_MS` (default
`20000`), `ABGEN_INDEX_BUILD_MAX_QUEUE` (default `0` = unlimited; when exceeded, new builds are skipped).
### LOD JIT lane
`ABGEN_LOD_JIT=1` (or `true`/`yes`/`on`) enables JIT LOD builds on `GET /LOD/{0|1}/...` misses; needs `gltfpack`
(`ABGEN_GLTFPACK` or `$PATH`), fails closed without it. Scenes without a published ISS descriptor run
through the embedded QuickJS scene runtime (feature `scene-runtime`); the `--manifest-builder` flag
and `ABGEN_LOD_MANIFEST_BUILDER` env are deprecated and ignored with a warning. Knobs:
`ABGEN_LOD_CACHE_DIR` (default: `ABGEN_CACHE_DIR`), `ABGEN_LOD_JIT_TIMEOUT_S` /
`ABGEN_LOD_JIT_FAIL_TTL_S` (defaults `600` / `3600`), `ABGEN_LOD_BUILD_CONCURRENCY` (default `1`),
`ABGEN_LOD_SUBPROC_TIMEOUT_S` (bounds each external tool run and the embedded scene run - the
runtime's interrupt makes the deadline a hard error), `ABGEN_LOD_SDK6_ADAPTION_URL` (overrides where
the runtime fetches the sdk6 adaption-layer bundle). Builds stage in a per-build workdir; only
gate-passed output is promoted into the serving root, so rejected bundles are never servable.
### Conversion / parity knobs
- `ABGEN_SHADER_BUNDLE` - path to `scene_ignore_windows` (default `crate/shader/scene_ignore_windows`)
- `ABGEN_CONTENT_ROOT` - local sharded content store root (default `./content`)
- compat: `ABGEN_V38_COMPAT`, `ABGEN_V38_TIMESTAMP`, `ABGEN_COLLECTION_MODE`, `ABGEN_REAL_TEXTURES`, `ABGEN_MAGENTA_MISSING`, `ABGEN_JPEG_TURBO_BOX`, `ABGEN_JPEG_GLB_9C`, `ABGEN_FAST_SERVE` (`live.rs::Proxy::new` sets some process-wide; not thread-safe before the first build)
- debug/dev: `ABGEN_BC7_CACHE`, `ABGEN_BC7_SCALAR`, `ABGEN_BC7_NO512`, `ABGEN_BC7_CAPTURE`, `ABGEN_LZ4_DUMP`, `ABGEN_TEST_CRN_OURS`/`_REF`/`_DUMP`
## License
AGPL-3.0-or-later (the Rust crate) - full text in [LICENSE](LICENSE). Vendored third-party: crunch =
Zlib, draco_decoder = MIT/Apache-2.0, libjpeg9c = IJG. Shader/template bundles are generated
Decentraland content artifacts (sha256-pinned; regenerable via `scripts/bootstrap-runtime.sh`). Note:
the lib sets `#[global_allocator] mimalloc`; any downstream embedding the lib inherits it.
