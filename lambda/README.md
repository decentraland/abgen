# abgen-lambda

The asset-bundle conversion pipeline as **one binary for AWS Lambda**: a
deployment event goes in; converted bundles and manifests go to the CDN
bucket; a finished-event goes to the asset-bundle-registry queue.

No async runtime — the Lambda runtime API is served with the same blocking
`ureq` the rest of abgen uses, and the handler is plain synchronous Rust
around `abgen::live::Proxy::build_entity_into_corpus`.

## Dual-emit

All configured platforms (default `windows,mac`) are built in one invocation.
Texture encoding — the dominant CPU cost — is platform-independent and cached
process-wide (`abgen::texencode_cache`), so the second platform reuses the
first platform's BC7/DXT encodes and costs roughly serialization only. The
per-entity hit/miss counts are logged and returned in the response. The cache
is otherwise opt-in (`ABGEN_TEX_ENCODE_CACHE=1` or
`abgen::texencode_cache::enable()`), stops inserting at
`ABGEN_TEX_ENCODE_CACHE_MAX_MB` (default 4096), and must be cleared between
entities. The Lambda handler enables it once and clears it after each entity.

## Status

| step | what | state |
|------|------|-------|
| 1 | texture-encode cache (dual-emit) | done |
| 2 | event parsing, `--once` local mode, dual-platform conversion | done |
| 3 | S3 publishing — abgen's native "space" writes bundles + manifests through during the build; no `.br` variants (no client of this pipeline fetches them) | done |
| 4 | registry SQS notification | deferred (registry duplicate is a follow-up) |
| 5 | already-converted skip (entity-level manifest check) **and** per-file asset reuse (space probe per digest-named glb) | done |
| 6 | container image (`nix build .#lambdaImage`) | done |
| 7 | LOD jobs — regenerated from the scene via `lodgen`, opt-in with `ENABLE_LODS` | done (FBX sources still not transcoded) |

## Local run (no AWS)

```bash
cargo build --release -p abgen-lambda      # workspace member; binary in target/release/
OUT_ROOT=/tmp/ab-out ./target/release/abgen-lambda --once lambda/examples/event-manual.json
```

Leaves the corpus under `OUT_ROOT/{entityId}/{platform}/` plus
`{platform}.manifest.json`, prints a summary JSON including the
texture-encode cache stats — with `windows,mac` the mac pass should be almost
all hits.

## Configuration (env)

| var | default | meaning |
|-----|---------|---------|
| `PLATFORMS` | `windows,mac` | build targets per entity, in order |
| `AB_VERSION` | `v49` | manifest/key version tag |
| `ABGEN_CACHE_DIR` | `$TMPDIR/abgen-cache` | content download cache (point at `/tmp` on Lambda) |
| `CONTENT_SERVER_URL` | foundation catalyst | fallback when the event carries none |
| `OUT_ROOT` | `$TMPDIR/abgen-lambda-out` | local conversion output root |
| `ABGEN_S3_ENDPOINT` | — | S3 endpoint (e.g. `https://s3.us-east-1.amazonaws.com`); **required to enable publishing** — unset leaves output on disk only |
| `ABGEN_S3_BUCKET` | — | CDN bucket name |
| `ABGEN_S3_REGION` | `AWS_REGION` → `us-east-1` | bucket region |
| `ABGEN_S3_PATH_STYLE` | off | path-style addressing (minio/localstack) |
| `ABGEN_S3_READ_ONLY` | off | probe/reuse without writing (dry runs) |
| `KEEP_OUTPUT` | off (`--once` forces on) | keep the local corpus after the run |
| `REGISTRY_QUEUE_URL` | — | registry queue (step 4, deferred) |
| `ENABLE_LODS` | off | generate LOD levels 0+1 for `lods` jobs instead of acking and skipping them (see [LOD jobs](#lod-jobs)) |
| `ALLOWED_CONTENT_SERVER_HOSTS` | — (**fail-open**) | comma-separated allowlist of hosts an event's `contentServerUrl` may name; **unset means any https host is accepted**, so every deployment should set it. The `lambdaImage` bakes in `peer.decentraland.org`; a function env var overrides it. |

S3 is abgen's built-in "space" client (SigV4, ureq). Credentials come from
the standard env (`AWS_ACCESS_KEY_ID`/`SECRET`/`SESSION_TOKEN`) or the
container credential endpoint — on Lambda/ECS the execution role provides
them automatically.

## CDN key layout (mirrors prod exactly)

| what | key |
|------|-----|
| scene bundles (canonical / asset-reuse) | `{AB_VERSION}/assets/{bundleName}` |
| wearable & emote bundles (entity-scoped) | `{AB_VERSION}/{entityId}/{bundleName}` |
| manifests | `manifest/{entityId}_{platform}.json` |
| scene sources (`main.crdt`, `scene.json`, main script; clean scene builds) | `{AB_VERSION}/{entityId}/{file}` |
| LOD bundles (+ `.br`), `ENABLE_LODS=1` only | `LOD/{level}/{sceneId}_{level}_{platform}` |
| ISS descriptor (+ `.br`), `ENABLE_LODS=1` only | `lods-unity/manifests/{sceneId}_InitialSceneState.json` |

## Container image & Lambda settings

`nix build .#lambdaImage` (`packages.lambdaImage` in the root flake — build
it on an aarch64-linux machine: Graviton is ~20% cheaper and abgen is
CPU-portable). The binary implements the Lambda runtime API itself, so no
AWS base image is needed; the result is a `docker-archive` tarball — push it
to ECR with skopeo (see the `lambda-image` job in
`.github/workflows/release.yml`) and point the
function at the image.

Release tags publish the image using GitHub OIDC. Configure repository
variables `ABGEN_LAMBDA_ECR_ROLE_ARN` with the push role's ARN,
`ABGEN_LAMBDA_AWS_REGION` with the role's AWS region, and
`ABGEN_LAMBDA_ECR_REPOSITORY` with the ECR repository URL — the OIDC step
in `release.yml` errors if the role ARN or region is unset.

Recommended function config:

| setting | value | why |
|---------|-------|-----|
| architecture | `arm64` | 20% cheaper compute |
| memory | 10240 MB | buys the max 6 vCPUs — texture encoding is CPU-bound |
| timeout | 900 s | the hard ceiling; worst-case scenes need the room |
| ephemeral storage | 10240 MB | `/tmp` holds the content cache + corpus |
| SQS trigger | batch size 1, visibility timeout ≥ 900 s, DLQ after ~3 receives | one entity per invocation; whales land in the DLQ |
| reserved concurrency | ~20 | politeness cap on catalyst downloads |

**Shader bundles:** none need seeding for unity-explorer — it resolves the
shared shader dependencies (`COMMON_SHADERS`) from its own embedded
StreamingAssets, never from the CDN. The vendored payloads still ship in
the image under `/opt/abgen/shader` (via `ABGEN_ROOT`) in case a
non-embedding client ever points at this bucket; such requests would
surface as `404`s in the CloudFront logs.

Bundles and manifests are written through to S3 by the build itself
(abgen's space); scene sources are published by the handler afterwards.
The space sets plain content types and no Cache-Control — **cache policy
must live on the CDN distribution**: long/immutable TTLs for
`{AB_VERSION}/…` (keys are content-addressed) and TTL 0 for `manifest/…`.
No `.br` siblings (see step 3 note above).

## Per-file asset reuse

Inside the build, every digest-named scene glb is HEAD-probed at
`{AB_VERSION}/assets/{name}` and skipped when it already exists (it still
appears in the manifest) — the consumer-server's `cachedHashes` mechanism,
natively. A redeployed scene with one changed model converts one model.
`"force": true` bypasses the entity-level manifest skip, but per-file reuse
still applies: existing canonical bundles are never overwritten (their keys
are content-addressed, so a differing bundle gets a different name).

## Event shapes accepted

SQS record batches whose bodies are catalyst `DeploymentToSqs` payloads
(`{"entity":{"entityId":…},"contentServerUrls":[…]}`), or a plain
`{"entityId":…, "contentServerUrl":…}` for manual invokes. LOD jobs
(`lods` present) take the [LOD lane](#lod-jobs). `"force": true` in either
shape bypasses the already-converted skip.

## LOD jobs

A deployment event with a `lods` array is a LOD job. Those URLs point at the
legacy Unity generator's **FBX** sources; abgen has no FBX importer, so it
does not transcode them. With `ENABLE_LODS=1` the handler instead
*regenerates* the LODs from the scene entity through the same `lodgen` chain
the abcdn server runs JIT (`abgen-lod generate`): resolve placements (ISS
descriptor, else the embedded scene runtime) → assemble → crop → atlas →
simplify → bundle, levels 0 and 1, for every configured platform that has a
LOD lane (`windows|mac|linux`; `webgl` is dropped with a log line). The
result passes the same structural self-gate as the JIT lane — a gate failure
fails the job and publishes nothing — and is then uploaded under the
unversioned `LOD/…` and `lods-unity/manifests/…` keys above.

```bash
ENABLE_LODS=1 OUT_ROOT=/tmp/ab-out ./target/release/abgen-lambda \
  --once lambda/examples/event-lods.json
```

Without `ENABLE_LODS` (the default) LOD jobs are acked and skipped with
`{"skipped": "lods-disabled"}`, i.e. LOD generation stays on the Unity
pipeline. Turn it on per environment: a LOD build is a whole-scene bake and
costs far more CPU/RAM/time than a single-entity conversion, so size the
function (memory, timeout, SQS visibility) for it first.

Known boundaries: level 2 is never emitted (production stopped emitting it),
the deployment's FBX source URLs are ignored rather than converted, and level
0 is the ISS-era pass-through bake rather than the retired legacy LOD0 shape
(same divergence `abgen-lod generate` documents).

## Already-converted skip

Before converting, each configured platform's `manifest/{entityId}_{platform}.json`
is fetched from the bucket. A platform is skipped when the manifest exists,
has `exitCode == 0` and matches the current `AB_VERSION` — the
consumer-server's `shouldIgnoreConversion` semantics. Partially-converted
entities rebuild only the missing targets; when everything is current, the
invocation returns `{"skipped": "already-converted"}` in seconds. Any
fetch/parse problem fails open (converts) — uploads are idempotent.
