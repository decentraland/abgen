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
| `ABGEN_SNS_TOPIC_ARN` | — (off) | SNS topic for `AssetBundleConversionFinished` events (see below) |
| `ABGEN_SNS_ENDPOINT` | `sns.{region}.amazonaws.com` | endpoint override (localstack); region comes from the ARN |
| `ABGEN_REDIS_URL` | — (off) | `redis://host[:port]` (or `rediss://…` for TLS) — enables the shared hit-cache in front of S3 existence probes (see below) |
| `ABGEN_REDIS_TTL_SECONDS` | `86400` | TTL on cached positive probes |
| `ABGEN_HTTP_SECRET` | — (**fail-closed**) | shared secret the Function URL POST path requires in `x-abgen-secret`; unset means every HTTP invocation is refused with `503` |
| `ENABLE_LODS` | off | generate LOD levels 0+1 for `lods` jobs instead of acking and skipping them (see [LOD jobs](#lod-jobs)) |
| `ALLOWED_CONTENT_SERVER_HOSTS` | — (**fail-open**) | comma-separated allowlist of hosts an event's `contentServerUrl` may name; **unset means any https host is accepted**, so every deployment should set it. Scheme/shape validation (https only, no userinfo) applies regardless — allowlist or not, a plaintext or internal-IP URL is rejected. The `lambdaImage` bakes in `peer.decentraland.org`; a function env var overrides it. |
| `ABGEN_EMF_NAMESPACE` | — (off) | CloudWatch namespace for EMF metrics (e.g. `abgen/lambda`); unset means no recorder is installed and every `metrics::` call stays a no-op |

S3 is abgen's built-in "space" client (SigV4, ureq). Credentials come from
the standard env (`AWS_ACCESS_KEY_ID`/`SECRET`/`SESSION_TOKEN`) or the
container credential endpoint — on Lambda/ECS the execution role provides
them automatically. The SNS client shares the same credential resolution.

## Finished events (SNS)

With `ABGEN_SNS_TOPIC_ARN` set, every terminal branch of a *conversion* job
publishes one `AssetBundleConversionFinishedEvent` per platform — the exact
shape and `type`/`subType` message attributes production's consumer-server
publishes (`consumer-server/src/adapters/sns.ts`), so a registry-side consumer
with `rawMessageDelivery: true` receives byte-compatible bodies:

- converted platforms carry their conversion exit code as `statusCode`
- already-converted skips publish `statusCode: 13`, matching prod's
  triage fast path — one event per processed job, and a redelivered SQS
  message re-notifies if an earlier publish failed after upload
- a successful `ENABLE_LODS=1` LOD job publishes one event per supported
  platform with `isLods: true` and `statusCode: 0`, mirroring upstream's
  `publishFinishedEvent(…, isLods: !!job.lods)`; the two LOD *skip* branches
  (`lods-disabled`, `lods-no-supported-platform`) ack the message and
  deliberately publish nothing — with LODs off, generation (and its events)
  stays on the Unity pipeline

Publish failures fail the invocation (SQS redelivers); with the ARN unset
nothing is published and the run reports `"notified": false`.

**This must be a dedicated topic, never the shared `event-driven-sns` bus**:
the prod registry's subscription filter matches every `asset-bundle` event,
and events from this pipeline describe a different CDN bucket. The message
attributes are what SQS filter policies match on — the body alone would be
dropped by every filtered subscription.

## Redis hit-cache (optional)

The same optimization production's consumer-server runs: Redis in front of
the S3 existence probes, with S3 staying the source of truth. Two probe kinds
get memoized, both only after an S3-confirmed positive:

- **per-file reuse probes** — before building a digest-named glb bundle, the
  build HEADs `{AB_VERSION}/assets/{name}`; a hit skips the build. On warm
  content that's one HEAD (~5–20 ms) per file, ~1000+ per large scene.
- **entity already-converted checks** — one manifest GET per platform per
  event, replayed on every redelivery of an already-converted entity.

Why it matters on Lambda specifically: invocations are billed as
memory × wall-time, and this function books 10 GB, so every second spent
waiting on S3 round-trips costs the same as a second of 6-vCPU encoding.
`batchSize: 1` also means no in-process memo survives across entities except
within a warm container, and up to 20 containers each re-learn the same
answers — Redis shares them across all of them, like consumer-server shares
probe hits across pods.

Semantics (mirrors consumer-server's asset-reuse cache):

- **positives only** — a missing object is never cached; a concurrent
  invocation may upload it at any moment.
- **fail-open** — any Redis error is a miss, the S3 probe runs as before, and
  the client backs off for 30 s so an outage can't make probes slower than
  no cache at all.
- keys are scoped to bucket (and version for entity markers), so caches for
  different CDNs can never cross-contaminate; a `force` job deletes the
  entity markers it bypasses — both before converting and again after its
  result is published — since a reconversion can downgrade a manifest.
- 24 h TTL bounds the keyspace across `AB_VERSION` bumps.

URL scheme picks the transport: `redis://` is plain TCP, `rediss://` wraps the
same RESP session in TLS (rustls) for clusters where ElastiCache in-transit
encryption is required. Certificates are verified against the webpki root
bundle — the Amazon-issued ElastiCache certificate chains to it, and there is no
opt-out for self-signed or hostname-mismatched certs. `user:password@` in either
scheme becomes an `AUTH` (2-arg for ACL users), so a cluster with an auth token
or a Redis ACL user works the same way over TLS.

Caveats on the entity markers specifically: unlike the content-addressed
per-file probe keys (immutable — a positive can never go stale), an entity
marker caches a *mutable* artifact, the manifest verdict. Two consequences:

- **residual force race** — a concurrent non-force redelivery that read the
  old `exitCode: 0` manifest between the force job's final publish and its
  marker delete can re-mark the entity, masking a downgraded force result for
  up to the TTL. The window is milliseconds and requires a concurrent
  redelivery of the same entity during a force reconversion; a second `force`
  clears it.
- **out-of-band reconversions** — force or manual reconversions done outside
  abgen (anything that rewrites `manifest/…` without going through this
  handler) leave the markers stale for up to the TTL.

Unset `ABGEN_REDIS_URL` and none of this exists — behavior is identical to
today's S3-only path.

Upstream equivalents, for anyone porting a deployment: the consumer-server
reads the same two knobs as `REDIS_URL` (`components.ts`) and
`REDIS_CACHE_TTL_SECONDS` (`scenes/component.ts`); the `ABGEN_` names follow
this repo's env var convention deliberately.

## Metrics (CloudWatch EMF)

Lambda has no metrics agent, so the counters the crate already records went
nowhere. With `ABGEN_EMF_NAMESPACE` set, the binary installs a `metrics`
recorder that accumulates in-process and, at the end of every invocation,
writes [Embedded Metric Format](https://docs.aws.amazon.com/AmazonCloudWatch/latest/monitoring/CloudWatch_Embedded_Metric_Format_Specification.html)
JSON lines to stdout — one line per label set. CloudWatch Logs turns those
into metrics with no API calls, no extra IAM permissions, and no dependency
beyond `serde_json`. `AWS_LAMBDA_FUNCTION_NAME` (set by Lambda) becomes a
`ServiceName` dimension; metric labels become the remaining dimensions.

| metric | dimensions | from |
|--------|------------|------|
| `abgen_lambda_invocations_total`, `abgen_lambda_invocation_duration_seconds` | `result` | runtime loop |
| `abgen_lambda_jobs_total`, `abgen_lambda_job_duration_seconds` | `outcome` (`converted`/`skipped`/`failed`/`error`) | handler |
| `abgen_lambda_convert_duration_seconds` | `platform` | per-platform build |
| `abgen_lambda_bundles_total` | `platform` | manifest entries written |
| `abgen_lambda_texencode_cache_total` | `outcome` (`hit`/`miss`) | dual-emit texture cache |
| `abgen_space_request_duration_seconds`, `abgen_space_transfer_bytes_total`, `abgen_space_object_bytes`, `abgen_space_errors_total` | `op`, `result`, `direction` | `abgen::live` S3 client |

Naming: the upstream consumer-server's registry
(`consumer-server/src/metrics.ts`, `ab_converter_*`) instruments a different
pipeline (task queues, triage, Unity exit codes); none of these lambda-side
concepts overlaps it one-to-one, so all names stay in this repo's `abgen_*`
namespace — deliberately, in upstream's style (snake_case, `_total`/`_seconds`
suffixes). Label conventions do follow upstream where a concept rhymes:
hit/miss counters use the label `outcome`, like upstream's
`ab_converter_glb_deps_cache_total{outcome ∈ hit,miss}`.

Histograms are emitted as a value array plus exact `_sum`/`_count` metrics.
EMF caps an array at 100 values, so longer runs are downsampled to 100
evenly spaced values of the sorted sample (min, max and percentile shape
survive; `_sum`/`_count` stay exact).

Deliberately out of scope: per-invocation dimensions such as entity id (a
metric dimension per entity would blow up CloudWatch custom-metric cost —
the entity id is already in the text log line next to it) and configurable
extra dimensions.

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
| SQS trigger | batch size 1, `ReportBatchItemFailures`, visibility timeout ≥ 900 s, DLQ after ~3 receives | one entity per invocation; whales land in the DLQ |
| reserved concurrency | ~20 | politeness cap on catalyst downloads |
| Function URL | auth `AWS_IAM`, or `NONE` with `ABGEN_HTTP_SECRET` set | ad-hoc POST-to-convert (see below) |

**Shader bundles:** none need seeding for unity-explorer — it resolves the
shared shader dependencies (`COMMON_SHADERS`) from its own embedded
StreamingAssets, never from the CDN. The vendored payloads still ship in
the image under `/opt/abgen/shader` (via `ABGEN_ROOT`) in case a
non-embedding client ever points at this bucket; such requests would
surface as `404`s in the CloudFront logs.

Bundles and manifests are written through to S3 by the build itself
(abgen's space); scene sources are published by the handler afterwards.
Every object carries the same Content-Type / Cache-Control the production
consumer-server writes, derived from the key by `space::object_headers`:
bundles are `application/wasm` + `public,max-age=31536000,immutable`
(cdn-uploader's comma-joined spelling), scene sources (`.js`/`.json`/`.crdt`)
the direct-upload spelling `public, max-age=31536000, immutable`,
manifests (`manifest/…`, `lods-unity/manifests/…`) are `application/json`
+ `private, max-age=0, no-cache`. That is origin-level defense in depth —
**cache policy still must live on the CDN distribution**: long/immutable
TTLs for `{AB_VERSION}/…` (keys are content-addressed) and TTL 0 for
`manifest/…`. No `.br` siblings (see step 3 note above), and no
`Content-Encoding` is set on the `.br` objects the LOD/bvwebgpu lanes do
write.

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
unversioned `LOD/…` and `lods-unity/manifests/…` keys above, followed by one
finished event per platform with `isLods: true` (see
[Finished events](#finished-events-sns)).

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

## Partial batch responses

An SQS batch answers in the event-source mapping's `ReportBatchItemFailures`
format — `{"batchItemFailures": [{"itemIdentifier": "<messageId>"}, …]}`,
empty when every record converted. A record that fails to parse or to convert
is reported by its own message id and redelivered alone; the rest of the batch
is deleted. Only a batch whose failing record carries no `messageId` fails the
whole invocation, since such a failure cannot be named (a real SQS record
always has one; hand-written `--once` fixtures may not).

Direct invokes — the console, `--once` with a non-`Records` payload — are not
part of the protocol and keep answering `{"jobs": [ … ]}`, failing the
invocation on error. Per-record summaries of an SQS batch go to the log.

Batch size is 1 today, which makes the two behaviours equivalent; the format
is implemented so raising it does not need a code change.

## POST-to-convert (Lambda Function URL)

An event carrying `requestContext.http.method` — the Function URL / API
Gateway payload format 2.0 shape — is handled as an HTTP request: the body
is the *same* JSON either shape above uses, and the reply is the same
summary, synchronously, inside an HTTP response envelope. This replaces
hand-crafting an SQS message for a one-off conversion.

```bash
curl -sS "$FUNCTION_URL" \
  -H 'content-type: application/json' \
  -H "x-abgen-secret: $ABGEN_HTTP_SECRET" \
  -d '{"entityId":"bafk…","contentServerUrl":"https://peer.decentraland.org/content","force":true}'
```

| status | when |
|--------|------|
| `200` | every job converted; body is the usual `{"jobs":[…]}` summary (or `{"batchItemFailures":[]}` for a `Records`-shaped body) |
| `400` | body is not JSON, or is not a recognized event shape |
| `401` | missing or wrong `x-abgen-secret` |
| `405` | method other than POST |
| `500` | the conversion failed. The body is a generic `error` — the failure chain goes to the function log, not over the wire. For a `Records`-shaped body, *any* failing record is a `500` whose body is the `batchItemFailures` summary: there is no queue on this path to redeliver them, so a non-2xx is the only signal the job was lost |
| `503` | `ABGEN_HTTP_SECRET` is unset — the POST path is disabled |

The conversion runs inline, so the caller waits for it and the function
timeout (900 s) is the request timeout; anything longer belongs on the queue.
Only the HTTP path answers in-band — SQS invocations still fail the
invocation on error so the queue can retry.

**Auth.** The Function URL itself should be `AWS_IAM` where the caller can
SigV4-sign (ops tooling, CI with a role); `NONE` is what makes an ad-hoc
`curl` possible at all, and then the shared secret is the *only* thing
between the internet and the converter. The secret check runs on every HTTP
invocation either way, is constant-time, and fails closed when unset, so
`NONE` + secret is a deliberate second factor rather than a substitute for
IAM. Set `ABGEN_HTTP_SECRET` from a secrets store, never in the image —
`lambdaImage` bakes `ALLOWED_CONTENT_SERVER_HOSTS` but must never bake this.
Rotate by setting a new value; there is no grace window for the old one.

Infra: one `aws.lambda.FunctionUrl` on the abgen function in the ops-lambdas
stack (that repo, not this one) plus the `ABGEN_HTTP_SECRET` env var.

Locally, `--once lambda/examples/event-function-url.json` exercises the whole
path (and exits non-zero when the envelope status is ≥ 400).

## Already-converted skip

Before converting, each configured platform's `manifest/{entityId}_{platform}.json`
is fetched from the bucket. A platform is skipped when the manifest exists,
has `exitCode == 0` and matches the current `AB_VERSION` — the
consumer-server's `shouldIgnoreConversion` semantics. Partially-converted
entities rebuild only the missing targets; when everything is current, the
invocation returns `{"skipped": "already-converted"}` in seconds. Any
fetch/parse problem fails open (converts) — uploads are idempotent.
