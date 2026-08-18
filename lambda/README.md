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
per-entity hit/miss counts are logged and returned in the response.

## Status

| step | what | state |
|------|------|-------|
| 1 | texture-encode cache (dual-emit) | done |
| 2 | event parsing, `--once` local mode, dual-platform conversion | done |
| 3 | S3 upload (SigV4 over ureq; no `.br` variants — no client of this pipeline fetches them) | done |
| 4 | registry SQS notification | TODO |
| 5 | already-converted / missing-files skip | TODO |
| 6 | container image (`provided.al2023`) | TODO |

## Local run (no AWS)

```bash
cargo build --release --manifest-path lambda/Cargo.toml
OUT_ROOT=/tmp/ab-out ./lambda/target/release/abgen-lambda --once lambda/examples/event-manual.json
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
| `S3_BUCKET` | — | CDN bucket; unset = leave output on disk only |
| `AWS_REGION` | `us-east-1` | bucket/queue region (Lambda sets it) |
| `S3_ENDPOINT` | — | custom S3 endpoint (minio/localstack; switches to path-style) |
| `S3_ACL` | — | e.g. `public-read` to mirror prod ACL buckets; unset for OAC buckets |
| `KEEP_OUTPUT` | off (`--once` forces on) | keep the local corpus after upload |
| `REGISTRY_QUEUE_URL` | — | registry queue (step 4) |

AWS credentials come from the standard env (`AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`) — on Lambda, the execution role
provides them.

## CDN key layout (mirrors prod exactly)

| what | key |
|------|-----|
| scene bundles (canonical / asset-reuse) | `{AB_VERSION}/assets/{bundleName}` |
| wearable & emote bundles (entity-scoped) | `{AB_VERSION}/{entityId}/{bundleName}` |
| manifests | `manifest/{entityId}_{platform}.json` |
| scene sources (`main.crdt`, `scene.json`, main script; clean scene builds) | `{AB_VERSION}/{entityId}/{file}` |

Bundle objects: `application/wasm`, `public, max-age=31536000, immutable`.
Manifests: `application/json`, `private, max-age=0, no-cache`. No `.br`
siblings (see step 3 note above).

## Event shapes accepted

SQS record batches whose bodies are catalyst `DeploymentToSqs` payloads
(`{"entity":{"entityId":…},"contentServerUrls":[…]}`), or a plain
`{"entityId":…, "contentServerUrl":…}` for manual invokes. LOD jobs
(`lods` present) are acknowledged and skipped — LOD generation stays on the
Unity pipeline.
