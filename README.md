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
cargo build --release --examples # bundle dump tools (objdump/texdump/matdump)
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
| `abgen-lod` | LOD lane CLI: `bundle`, `placements`, `assemble`, `atlas`, `simplify`, `generate`, `compare` (structural diff of two LOD bundles: materials/textures/vertices/triangles plus a per-material property diff — shader, keywords, render queue, floats, clipping colours, texture bindings; local or CDN URL), `qualify-corpus` (Genesis City + Worlds corpus build; `--reference-cdn` records the same diff against production per scene) |
| `abgen-lambda` | AWS Lambda handler (deployment event in, bundles + manifests to S3; from the `lambda/` workspace member) - see [lambda/README.md](lambda/README.md) |

Plus bundle-inspection examples (`texdump`, `matdump`, `objdump`) under
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
LOD assembly frame conformance against the production `_1.glb` references: `cargo test --release
--test lod_handedness` (offline fixtures; add `-- --ignored` to assemble from the catalyst).

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
The default `server` feature builds the complete service. Reusable consumers should select only the lanes they need; the smallest native converter is `cargo build --release --lib --no-default-features`.

| Feature | Adds | Enabled by |
|---|---|---|
| `server` (default) | HTTP/registry stack, `gpu`, `cli`, `web-pack`, and `scene-runtime` | `abgen` server |
| `gpu` | native wgpu/CUDA dispatch | server, build/corpus/verify CLIs, Lambda |
| `web-pack` | BV WebGPU pack emission plus DDS/glTF JSON dependencies | server, build/corpus CLIs |
| `bevy-variants` | **prototype** Bevy variants lane (per-entity manifest of BC KTX2 / GLB variants named by source CID and encoder generation, reused across entities); implies `web-pack` | server |
| `scene-runtime` | QuickJS fallback for suspicious static LOD placements; each scene runs in a short-lived child process (seccomp-confined on Linux; separate process and empty environment elsewhere). Binaries must call `scenerun::run_sandbox_child_if_requested()` first thing in `main`; `abgen --scene-sandbox-check` proves a deployment can run scenes. Where a Linux runtime refuses seccomp (AWS Lambda rejects `PR_SET_NO_NEW_PRIVS`), the default policy refuses scenes; `ABGEN_SANDBOX_SECCOMP=auto` runs them in the separate process without the filter and warns once | server, LOD CLI, Lambda |
| `cli` | tracing subscriber and CLI logging | server and LOD CLI |

`crate/abgen-native`, `crate/abgen-node`, and converter WASM use no default features. The standalone `crate/wasm-gpu` package compiles the shared BC7/WebGPU source directly and does not depend on the full `abgen` graph; its worker ships only `gpu_init` and `gpu_encode`. Static CRDT placement interpretation remains available without `scene-runtime`; if that result is empty or suspicious, the minimal build fails explicitly instead of publishing an empty LOD.

GPU backends self-qualify per device at enable time: the selected backend (`auto` tries CUDA, then
wgpu) must reproduce the CPU BC7 encoder bit-for-bit on a probe matrix (sizes x srgb x perceptual x
profile, full mip chains) or it is disqualified and `--gpu`/`ABGEN_GPU=1` exits 2 instead of silently
degrading - a qualified GPU run produces byte-identical output to the CPU path. `ABGEN_GPU_QUALIFY=0`
skips the probe.

The slim, async-free converter lib + CLIs are the `wasm32-unknown-unknown` target build (the server,
content-DB, and GPU stacks are `cfg(not(target_arch = "wasm32"))`).
## Toolchain
- rustc/cargo (edition 2021; the vendored `draco_decoder` is edition 2024)
- cc + a C++ toolchain (vendored crunch, libjpeg9c; the vendored KTX2 transcoder `basisu` is pure Rust)
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
- `template/font-types.mac.bundle` - the font lane's donor (Unity 6000.5 `Font`, `TMP_FontAsset` and UI Toolkit
  `FontAsset`), built by `template/src/FontTypesTemplate.cs`; scene `.ttf` files convert to font bundles
  whose atlases come pre-filled with ASCII and common Latin-1 glyphs (`crate/src/fontgen`)
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
| `ABGEN_UPSTREAM_AB_REGISTRY` | see right | upstream registry URL: records for entities without local bundles are answered with its versions. Default: `asset-bundle-registry.<domain>` when the CDN is `ab-cdn.<domain>`. `off` disables |
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

Uploads carry each key family's production writer's object metadata, derived from the key by
`space::object_headers`: bundles `application/wasm` + `public,max-age=31536000,immutable`
(cdn-uploader's comma-joined spelling), scene sources (`.js`/`.json`/`.crdt`) the direct-upload
spelling `public, max-age=31536000, immutable`, manifests (`manifest/…`) `application/json` +
`private, max-age=0, no-cache`, ISS descriptors (`lods-unity/manifests/…`)
`public, max-age=31536000` (lod-generator-unity's spelling).

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

### Two version lanes: `AB_VERSION` and `WEARABLE_AB_VERSION`
Scene bundles and wearable/emote bundles live under separate key prefixes. `AB_VERSION` is the scene
lane; `WEARABLE_AB_VERSION` is wearables and emotes, and defaults to `AB_VERSION` when unset — so a
deployment that does not set it behaves exactly as it did when the two shared one prefix.

They are split because only a scene's name can carry an invalidation. Scene bundles are digest-named
(`{hash}_{digest}_{platform}`), so a dependency change or a recipe bump moves them; wearable bundles
are `{hash}_{platform}`, with nowhere to put one, which leaves the prefix as the only lever they
have. Sharing one prefix therefore meant every scene-driven bump dragged the whole wearable corpus
through a rebuild it had no use for, and every wearable-driven bump did the same to the world.

- The conversion path resolves its lane from the entity type (`Proxy::version_for`); everything
  that is not a scene rides the wearable lane.
- The serving path has only a bundle name to go on, so it reads both lanes and then
  `ABGEN_FALLBACK_VERSION` (`Proxy::read_versions`). Safe rather than merely convenient: an
  entity's bundles exist under exactly one prefix and every name is content-addressed, so a hit is
  the right bytes whichever lane answered. A miss costs one extra 404.
- The lambda's already-converted gate accepts a manifest at *either* lane, because the entity type
  is only known after the entity doc is fetched and the gate deliberately runs before that. Safe
  while the two are distinct strings: a manifest carries whichever version wrote it, so a bump of
  either stops matching.

### Per-asset-type cache keys (recipes)
`AB_VERSION` prefixes every space key, so bumping it orphans every bundle of every type at once —
the right hammer for a change to the bundle container, far too big for the usual fix, which changes
how one kind of asset is built and leaves the rest byte-identical. Baking skinned-renderer bounds
over the animation clips (#119) rewrote nothing but animated rigs; an `AB_VERSION` bump behind it
would have rebuilt every texture in the world.

`crate/src/recipes.rs` holds a generation counter per build-affecting behaviour and folds the ones
an asset uses into the digest that names its bundle. A bundle is rebuilt whole or not at all — a GLB
bundle carries its meshes, skeleton, clips, materials *and* its resolved textures in one artifact —
so there are only two base recipes, one per kind of bundle, plus narrowing ones that earn their
place by being rare:

| Recipe | Applies to | Rebuilds on a bump |
|---|---|---|
| `glb` | every glTF | every glb bundle; no standalone image bundles |
| `texture` | standalone images, and any glTF with `images` | every image bundle **and** every glb that embeds one |
| `skin` | a glTF declaring `skins` | only rigs — a minority of glbs |
| `animation` | a glTF carrying `animations` | only animated glbs — a minority |
| `ktx2` | a glTF declaring `KHR_texture_basisu` or carrying a KTX2 image | only KTX2-bearing glbs — rare |

**To ship a fix without an `AB_VERSION` bump:** raise the matching counter by one, in the commit that
changes the output, then re-enqueue as you would for a version bump. Bundles whose recipes moved get
new names, miss the HEAD probe and rebuild; every other bundle keeps its name and is reused where it
already sits. Never renumber a counter and never reuse a value — a generation that comes back around
makes stale bundles look fresh.

Default to `glb`. Reach for `skin` or `animation` only when the fix has a gate you can point at —
#119 is the model, where a glTF with no clips serialized byte for byte as before. The asymmetry is
the whole rule: too broad costs a rebuild you were going to pay anyway, too narrow ships stale
bundles and says nothing. There is deliberately no counter per sub-asset (`mesh`, `material`) — the
bundle is the unit, so those would rebuild the same set `glb` does — and none for normal maps, since
"is this encoder fix normal-only?" is exactly the ambiguous question a recipe must not ask.

Scope and cost:
- Only the digest-named lane carries recipes. Wearables and emotes are `{hash}_{platform}`, with
  nowhere to put a generation, so a fix to those still needs `AB_VERSION` — as does anything that
  changes the bundle container, the manifest shape, or the client's side of the contract.
- Generation `0` folds in as nothing *in a digest*, so adopting this cost no rebuild: every
  bundle name is byte-for-byte what it was before. Manifests do gain a `recipes` key.
- `AB_VERSION` stays total. It is the key prefix, never a digest input, and the conversion gate
  gained a conjunct rather than a substitute, so a version bump still reconverts everything.
- Each conversion records, in the per-platform manifest's `recipes` block, every recipe governing
  its bundles and the generation each stood at — **baselines included** — and `platform_converted`
  compares it, so the lambda's already-converted skip stays correct without a `force` flag. A scene
  of static props records `{"glb":0,"texture":0}`: bumping `skin` leaves it current (that is the
  saving), bumping `glb` does not. Recording only the *bumped* recipes would be blind to every
  later `0 -> 1` bump, which is why the baselines are kept here and dropped in the digest.
  A manifest predating recipes has no block at all and reconverts once.
- The LOD lane solves the same problem with one counter of its own,
  `crate/src/lodgen/pipeline.rs::LOD_GENERATION`, folded into the reuse state document.

### KTX2 textures (`KHR_texture_basisu`)
glTF textures stored as Basis Universal **UASTC** KTX2 are decoded to RGBA8 (`crate/src/ktx2.rs`, on the
vendored pure-Rust `basisu` transcoder) and then take the same lanes as a PNG of the same picture:
BC7 on desktop, DXT5 on webgl, full mip chain, the 1024/512 platform caps, normal-map packing, and
sRGB vs linear decided by material usage. Scope and rules:
- Taken: a single 2D UASTC LDR image, supercompression none or Zstandard, at most 8192 on a side.
  Rejected (decoded as `None`, like an unreadable PNG: an empty slot, or magenta under
  `ABGEN_MAGENTA_MISSING`): ETC1S/BasisLZ, UASTC HDR, cubemaps, arrays, volumes, anything larger.
- A texture's core `source` always wins; `extensions.KHR_texture_basisu.source` is read only when
  `source` is absent. A file shipping a PNG/JPEG fallback builds from exactly the bytes it did before.
- `.ktx2` is convertible like `.png` (standalone image bundles keyed `.ktx2`) and is a GLB dependency
  extension, so replacing a referenced `.ktx2` renames the GLB bundle.
- The legacy Unity converter never decoded KTX2, so there is no upstream output to match: giving a
  decoded KTX2 the platform cap instead of the 2048 importer default is a policy choice.
- Bundles that might have shipped without their KTX2 texture are renamed by the `ktx2` recipe
  (introduced at generation 1). Manifests written before it cannot name it, so the lambda's
  already-converted gate still reads those entities as current: re-enqueue them with `force: true`.

Display-only files — `autogenerated-thumbnail.png` anywhere in the content map, and whatever
`metadata.display.navmapThumbnail` points at — are dropped from the conversion input, matching the
prod converter's `-skippedHashes` flag: no bundle, no manifest entry, and no contribution to the
manifest `exitCode` (the client reads those files as raw bytes from the content server, so a bundle
for them is a dead artifact). Applies to the JIT server and the lambda, independent of asset reuse.

### Registry index eager build
On `POST /entities/active|versions`, entities missing a converted bundle are queued for conversion; the
request waits up to the deadline, builds finish in the background. Knobs: `ABGEN_INDEX_EAGER_BUILD`
(default on; `0`/`false`/`no` disables), `ABGEN_INDEX_BUILD_PLATFORMS` (default `windows,mac`),
`ABGEN_INDEX_BUILD_CONCURRENCY` (default: CPU count), `ABGEN_INDEX_BUILD_DEADLINE_MS` (default
`20000`), `ABGEN_INDEX_BUILD_MAX_QUEUE` (default `0` = unlimited; when exceeded, new builds are skipped).
### Bevy variants lane (prototype)
Per entity (scene, wearable or emote), a manifest mapping each original content CID to the paths
of its GPU-ready variants for the Bevy explorer. A path's prefix letter is its class (`t` BC KTX2,
`m` `glb-br`, `g` GLB); the KTX2 header carries the BC format and size. The full entry of a
variant (its CID, format, dimensions, size) lives in its record beside it, which is never served:
a key's bytes are immutable within its generation and served `immutable`, so the client trusts
the path. The client loads a variant when the manifest lists one it supports and falls back to
the original file on anything else, so the lane can be switched off at any time. Built by
`crate/src/bevyvariants` (feature `bevy-variants`, on with `server`).

- `GET /bevy/manifest/{entity}.json` - `{"v":3,"lanes":{"glb":"g4","mesh":"m2","texture":"t4"},"files":{"<original cid>":["bevy/t4/<original cid>_srgb"]}}`
  (at most one path per class per original: a texture, a `glb-br` and a `glb`, in order of
  preference: clients load the first they support, so a `.glb` lists its `glb` variant before
  its `glb-br` copy). 404 while absent, and while every copy on disk was built from an older
  generation or schema (`X-Abgen-Reason: bevy-variants-stale`, see the asset lane below).
  Nothing is built on request: the conversion lambda builds the lane for each entity after its
  asset bundles (`lambda/src/bevy.rs`), and `abgen-corpus --bevy-variants` builds it offline. The
  client loads the original files until a manifest exists.
  `Cache-Control: private, max-age=0, no-cache`.
- `GET /bevy/{prefix}/{name}` (an entry's `path`) - variant bytes, `public,max-age=31536000,immutable`,
  `Range`, `ETag`, CORS (preflight allows `X-IPFS`). Never built on its own: it exists once a
  manifest naming it does. When the variant has a Brotli sidecar (`{name}.br`, see `glb-br`
  and GLB variants below) and the request's `Accept-Encoding` includes `br`, the sidecar's bytes are sent with
  `Content-Encoding: br`; such responses carry `Vary: Accept-Encoding` either way. The CID is always
  of the plain bytes. The record beside a variant (`{name}.json`) is never served.
- With the S3 space configured, a miss on either route reads through from the same keys
  (`bevy/manifest/…`, `bevy/{prefix}/…`, and `{name}.br` when the client accepts `br`), so a
  corpus output synced to the bucket is served as is. Files read through count against the JIT
  disk budget one by one; an evicted one is read through again. The server never writes these
  keys: the conversion lambda does. `ABGEN_BEVY_VARIANTS=0` turns the read-through off.

**Asset lane.** A variant's name is a function of its inputs, so a build knows the name before
doing any work and looks for its record the way the Unity lanes probe
`{version}/assets/{hash}_{digest}_{platform}` - no fetch, no decode, no hash of anything:

- textures: `bevy/t{TEX_GEN}/{sourceCid}_{slot}`, `slot` = `srgb` | `linear` | `normal` (how the
  entity's models sample the image; the same image used differently in two scenes is two
  variants). A Zstandard level other than the default is part of the prefix (`t4z9`).
- `glb-br` models: `bevy/m{MESH_GEN}/{sourceCid}` (a `.gltf` source only ever gets a `not-glb`
  record there).
- GLB variants: `bevy/g{GLB_GEN}/{sourceCid}`; a `.gltf` source gets a digest of the
  external `.bin` buffers it inlines appended (`{sourceCid}_{16 hex}`), the same `.gltf` beside
  different `.bin` files being two variants. Image URIs are left untouched, so they are not inputs.

Beside every variant sits its record, `{name}.json`: the manifest entry (`variant`) or the reason
there is none (`reason`: `tiny`, `decode-error`, `not-an-image`, …, so a file that cannot have a
variant is not retried by every entity that references it), whether a `.br` sidecar exists
(`br`), and on model records how the model samples each image URI (`uses`), so a warm entity
needs no model bytes to pick its textures' slots.

The store-backed build (`Proxy::build_bevy_variants`, run by the conversion lambda once per
entity) works like the bundle lanes. Unless forced, an entity whose manifest in the store is
current is skipped outright. Otherwise it probes each key's record in order: its output
directory (a record whose variant file is gone is a miss and is deleted), Redis
(`abgen:hit:{bucket}:{key}.json`, the record is the marker's value, 24 h, positives only,
fail-open), the store (one GET, then marked in Redis). A hit fills the manifest row; a miss
fetches and encodes. Outcomes that depend on the run (content missing, a failed write) are never
recorded. Writes go variant bytes, `.br`, record (then its Redis mark), and the manifest last, so
the store never names an object that is not there; a key whose objects did not all land is
dropped whole, and if any upload fails, or any file could not be fetched or encoded, the
manifest is not written (what did build is still uploaded): no manifest means the client uses
the originals, and the next conversion of the entity tries again. Records left in its output directory are not trusted (an earlier run
may have died before uploading). The offline build (`abgen-corpus --bevy-variants`) uses only the records
in its output directory.

Versioning is three integers in `crate/src/bevyvariants/mod.rs`: `TEX_GEN` (BC encoders, levels,
size cap, KTX2 writer, UASTC transcode), `MESH_GEN` (model variants: 2 = the original `.glb`
plus its `.br` sidecar) and `GLB_GEN` (GLB variants: their rewrite rules and the encoders inside,
so it moves whenever `TEX_GEN` does; 4 = no size gate on embedded images plus `.br` sidecars).
`TEX_GEN` 4 drops the size gate. Bump one when its encoder's output bytes change: every key of that class moves to a new
prefix, the previous prefix (`bevy/t1/`, …) is garbage and can be deleted whole, and manifests
built from it (their `lanes` name it) read as stale until the entity's next build, which re-encodes only that class.
The generations live in code because they describe the encoders. The operator's lever is
`ABGEN_BEVY_EPOCH` (unset by default; a number `0..=9999`): when set, every prefix gets `e{N}`
appended (`t4e2`, `m2e2`), so every key misses, every manifest reads as stale and the lane
re-encodes into the new prefixes, without a release. Setting it back finds the old prefixes again.
No per-recipe digests: skins, animations and textures referenced by a model do not change the
model variant's bytes, so they are not in its name. The option toggles
(`models`, `glb_variants`, `precompress_br`) are not generations: a reused `glb-br` variant whose
record has no `.br` sidecar gets one added, and turning precompression off stops building model
variants (GLB variants are still built, without sidecars) but leaves existing sidecars in place.

Variants (all textures are single-level KTX2 - level 0 only, no mipmaps - with UNORM BC blocks,
Zstandard-supercompressed, `KTXorientation` `rd`; writer in `crate/src/ktx2_write.rs`). Zstandard is libzstd 1.5.7, bundled by
the exactly pinned `zstd-sys` (`=2.0.16`): one single-threaded frame per level, no dictionary, no
checksum. The level is 19 unless set (`ABGEN_BEVY_ZSTD_LEVEL` on the server, `--bevy-zstd-level`
on `abgen-corpus`, 1..=19). Variant bytes, and so CIDs, depend on the level and the libzstd
release, never on the machine or thread count; a new libzstd release is a `TEX_GEN` bump.
- PNG/JPEG textures (sniffed by bytes): level 0 only, no mip chain (like the single-level
  creator UASTC files, so sizes compare like for like), longest side capped at 1024 and snapped
  to powers of two (whole 4x4 blocks). `ktx2-bc5` for images the entity's models use only as
  normal maps (x in R, y in G - Bevy's two-channel normal path), `ktx2-bc1` for fully opaque
  images, `ktx2-bc7` otherwise. An opaque colour (sRGB) image keeps BC1 only when its BC1
  level 0, decoded, is within 36 dB PSNR (RGB) of the source at the variant size; below that
  (posters, text, hard-edged art) it gets `ktx2-bc7` instead (`bc1_rejected` in the stats).
  Linear (metallic-roughness, occlusion) images keep BC1. Images 2 px or less on a side get none.
- No size gate: a variant larger than its original file is still served. The explorer never
  draws a PNG/JPEG as is: it decodes it to RGBA8 (4 bytes per pixel) and converts it to BC on
  the player's CPU, so dropping a variant to save a little download costs the full memory on a
  first visit. On Genesis Plaza's 285 standalone PNGs the gate dropped 54 files (19% of the
  pixels); converted anyway they grow about 1.2x on the wire.
- creator UASTC KTX2 (standalone or embedded in a GLB): a pure block-for-block UASTC -> BC7
  transcode (`ktx2-bc7`) of one level only - the first level no larger than 1024, at its own
  size, exactly what the explorer's local path transcodes (same vendored basisu sources, one
  level, no mips) - so remote and localhost give the same texture. No resize, re-encode, BC1/BC5
  or mips: a file that would need a resize or re-encode gets no variant. Shipping the file's
  lower mips would need the explorer's local transcode to keep them too (a separate change).
- GLB variants (format `glb`, class `g`, generation 4), **on by default**
  (`ABGEN_BEVY_GLB_VARIANTS=0` on the server, `--bevy-no-glb-variants` on `abgen-corpus`): every
  `.glb`/`.gltf` with something to change becomes a GLB with Draco decoded, `KHR_texture_basisu`
  resolved to a core `source` (an existing PNG/JPEG `source` wins), external buffers inlined and
  every embedded image replaced by a BC KTX2 (`image/ktx2`, the format rules above, no
  size gate either: the explorer holds a PNG/JPEG as RGBA8, 4 bytes per pixel, against 0.5 for
  BC1 and 1 for BC5/BC7, and a JPEG's BC KTX2 is often larger than the JPEG). An embedded PNG/JPEG that gets no encode (tiny, undecodable, not an image)
  keeps its bytes; a GLB whose embedded KTX2 does not transcode (`embedded-ktx2-kept`) or whose
  textures sample an external KTX2 (`external-ktx2`) gets no variant, so every KTX2 in a GLB
  variant is BC1, BC5 or BC7. Images no texture references are dropped, each distinct embedded
  image is encoded and stored once, and a model over 512 distinct images or 2^28 source pixels
  (`over-limits`) or whose materials do not classify (`unclassified`) gets no variant.
  Standalone and embedded PNG/JPEGs over 2^28 pixels by their header get none (`too-large`). Images embedded in
  GLBs never get a texture variant of their own; external image URIs are left as they are. A
  model nothing changes in gets no variant (`nothing-to-change`); models whose meshes are
  compressed by an extension other than Draco are not handled yet. With precompression on, a
  GLB variant also gets a Brotli `{name}.br` sidecar (quality 5: the zstd KTX2 inside does not
  compress, so a higher quality only costs CPU) when it is smaller (the geometry inside
  still compresses about 2x); the variant is kept either way. The stats count GLB variants,
  embedded images converted per format and source, images kept and why, and the embedded
  images' GPU bytes before and after (`glb_variants` in `BEVY_STATS`, one `glb_files` row per
  model with its images in `bevy-variants-stats.json`).
- `glb-br` models (format `glb-br`, class `m`, generation 2), **on by default**
  (`ABGEN_BEVY_MODELS=0` on the server, `--bevy-no-models` on `abgen-corpus`, independent of the
  GLB variants above): every `.glb` gets a variant that is the original `.glb`, byte for byte,
  plus its Brotli sidecar `{name}.br` (quality 9, window 22) for the route above. Geometry is not
  re-encoded, so the client needs only its glTF loader. The variant is kept only when the sidecar
  is smaller than the original (`size-gate` otherwise); a `.gltf` gets none (`not-glb`). Without
  precompression (`ABGEN_BEVY_PRECOMPRESS=none` or `--bevy-precompress none`) no model variants
  are built, since the sidecar is all they add. Standalone KTX2 variants are never Brotli'd. On a
  read-through miss the edge fetches the sidecar beside the variant when the client accepts `br`.
- nothing for scene code, source maps, audio, video or display-only thumbnails.

Rows are top-down (Bevy and glTF's top-left UV origin), unlike the Unity lanes, which flip.
Prebuild a corpus the server serves as is:
```bash
abgen-corpus --world <name> --worlds-url https://worlds-content-server.decentraland.org --bevy-variants ./out
abgen-corpus --entity-ids ids.txt --fetch-missing --bevy-variants ./out   # --bevy-no-glb-variants skips GLBs
ABGEN_OUT_ROOT=./out ./target/release/abgen   # serves /bevy/manifest/... and /bevy/<prefix>/...
```
A second run into the same `./out` (or another entity sharing files) reuses every variant whose
record is there and encodes only the rest. `abgen-corpus --bevy-variants` prints one
`BEVY_STATS {json}` line per entity (stage times summed over files - probe, fetch, decode, resize,
BC1/BC5/BC7 encode, the BC1 PSNR check, UASTC->BC7 transcode, zstd, brotli,
hash+write - counts and bytes by format and source, no-variant reasons, BC1 rejections, `reused_textures` / `reused_models`, and the `model_variants` totals:
models, variant and `.br` bytes, size-gated attempts; and the `glb_variants` totals) and writes the full stats, per-file rows included, to
`./out/bevy-variants-stats.json`.
### LOD JIT lane
`ABGEN_LOD_JIT=1` enables JIT LOD builds on `GET /LOD/1/...` misses (level 1 only, the production
shape: level 0 is the client-side ISS assembly, so `LOD/0/...` answers `lod-level-unsupported` and
`abgen-lod generate --level 0,1` is the offline opt-in); needs `gltfpack`
(`ABGEN_GLTFPACK` or `$PATH`), fails closed without it. Knobs: `ABGEN_LOD_MANIFEST_BUILDER` (unset
limits JIT to scenes with a published ISS descriptor), `ABGEN_LOD_CACHE_DIR` (default:
`ABGEN_CACHE_DIR`), `ABGEN_LOD_JIT_TIMEOUT_S` / `ABGEN_LOD_JIT_FAIL_TTL_S` (defaults `600` / `3600`),
`ABGEN_LOD_BUILD_CONCURRENCY` (default `1`). Builds stage in a per-build workdir; only gate-passed
output is promoted into the serving root, so rejected bundles are never servable. A passed build is
written back to the space with the production key set: `LOD/1/{sid}_1_{platform}`,
`lods-unity/manifests/{sid}_InitialSceneState.json` and the gltfpack-layout
`lods-unity/lods/{sid}_1.glb`.
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
`nix build .#lambdaImage` produces its container image, which the `lambda-image-push` job pushes to ECR
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
