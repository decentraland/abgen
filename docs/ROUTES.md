# Route parity: abgen vs the production ab-cdn + asset-bundle-registry

What `abgen` serves versus the two production surfaces it replaces when an explorer points its
unified `optimized-assets` base at one host: the static ab-cdn bucket and the
asset-bundle-registry service.

## Asset delivery (production ab-cdn)

Upstream is a pure static bucket behind a CDN. abgen forwards `Range` and `Origin` semantics and exposes `ETag`/`Content-Range`/`Accept-Ranges` on GET/HEAD.

| Route | Upstream | abgen |
|---|---|---|
| `GET /manifest/{entity}_{platform}.json` | static object, cacheable | served from `ABGEN_OUT_ROOT`, JIT-converts on miss; `Cache-Control: private, max-age=0, no-cache` |
| `GET /{version}/{entity}/{file}` | static object | served, JIT on miss; `Cache-Control: public,max-age=31536000,immutable` |
| `GET /{version}/{file}` (flat, incl. `{hash}_{platform}`) | static object | served; unresolvable flat hashes are negative-cached (`ABGEN_HASH_RESOLVE_FAIL_TTL_S`) |
| `GET /LOD/{level}/{file}` | static object | served; JIT only with `ABGEN_LOD_JIT=1` + `gltfpack`, otherwise 404 until primed |
| `GET /lods-unity/manifests/{scene}_InitialSceneState.json` | static object | served, with an ISS JIT fallback |
| `GET /bvwebgpu/{profile}/{entity}.pack` | browser pack object | served/generated as a range-friendly uncompressed object |
| `GET /bevy/manifest/{entity}.json` (prototype) | none | Bevy variants manifest; 404 until built (by the conversion lambda per entity, or offline with `abgen-corpus --bevy-variants`; nothing is built on request), and 404 `X-Abgen-Reason: bevy-variants-stale` while every copy on disk was built from an older generation; S3 read-through; `no-cache`. See README, *Bevy variants lane* |
| `GET /bevy/{prefix}/{name}` (prototype) | none | variant bytes (BC KTX2 with Zstandard levels, or GLB) at the `path` its manifest entry names (keys are named by the variant's inputs, not its CID); `immutable`, `Range`, CORS incl. `X-IPFS` preflight; S3 read-through; a `.br` sidecar is sent with `Content-Encoding: br` to clients that accept it |
| shader bundles (`scene_ignore_{windows,mac}`) | 404 (purged upstream) | self-primed from the vendored copies on first request |

Header semantics abgen implements natively: `ETag` with `If-None-Match` 304; `Range` 206/416 with
`Content-Range` (responses up to 8 MB buffered, larger streamed; `ABGEN_MAX_RANGE_BUFFER_BYTES`);
CORS `*` exposing `ETag`/`Content-Range`/`Accept-Ranges`/`Content-Length`.

Deltas an operator must know:

- Outside the Bevy variants lane, abgen does not generate, serve, or negotiate Brotli sidecars. Web deployments can apply HTTP compression at their own edge without changing abgen object keys. The Bevy lane's `glb-br` model variants (the original `.glb` plus a Brotli `.br` sidecar, on by default; `ABGEN_BEVY_MODELS=0` or `ABGEN_BEVY_PRECOMPRESS=none` turns them off) are negotiated by `Accept-Encoding`.
- Manifests are `no-cache` where upstream serves a cacheable TTL. A fronting CDN must forward
  `Range` and honor the `no-cache`.

## Registry (asset-bundle-registry)

The source contract is pinned to `decentraland/asset-bundle-registry`
`63c9ef2bd7365ac7312d8bd23b6759515ca999e5`. The table describes route availability
in the current tree; it does not assert complete upstream wire parity.

| Route | abgen behavior |
| --- | --- |
| `GET /status` | Registry status with nested service metadata and a millisecond timestamp. |
| `POST /entities/active`, `POST /entities/versions` | Native conversion records over the configured Catalyst entity source; both accept `world_name`. Require Registry persistence. Active reads exclude denylisted entities; versions retains them, as the pinned source does. |
| `GET /entities/status/{id}` | Native conversion status; accepts entity ID or pointer and optional `world_name`. |
| `GET /entities/status` | Signed-fetch inventory of registered entities found through the shared deployer lookup; worlds deployer inventory remains incomplete. |
| `GET /queues/status` | Native pending mac/windows job IDs from `build_jobs`. |
| `POST /profiles`, `POST /profiles/metadata` | Mounted by the unsigned abgen component over the shared entity source. |
| `GET /worlds/{worldName}/manifest` | Converted world availability with native denylist and spawn policy. |
| `GET /denylist` | Public native moderation entries; returns an empty array when persistence is not configured. |
| `POST/DELETE /denylist/{entityId}` | Signed moderator writes to native persistence; admin bearer alone is not accepted. |
| `POST /registry` | Mounted with a nonempty admin token; reconciles local entity/manifests and persists conversion/fallback state. |
| `DELETE /flush-cache` | Mounted with a nonempty admin token; acknowledges fresh job-status reads without deleting durable jobs or manifest caches. |

Additional operations include bearer-protected `/queues/retry`, `/queues/pause`
and `/queues/resume`, plus `/ping`, `/health`, `/livez`, `/readyz` and `/metrics`
(the last is bearer-gated when `ABGEN_METRICS_BEARER_TOKEN` is set).
The queue worker converts in private staging and commits only current-generation
facts with its queue receipt. It recovers abandoned claims and leaves artifact
delivery to HTTP JIT; queue conversion does not warm served files or Space. These
additional queue controls are not in the pinned Registry router.

`GET /health` includes this binary's crate version and process ID for loopback
supervision. Its `content_db` label describes the optional converter content-DB
connection; `registry` reports `catalyst-proxy`. These labels alone do not prove
native Registry database readiness.

## Registry authority and persistence

Registry entity lookups use `ABGEN_CATALYST_URL` and the configured worlds URL.
These should name the authoritative content/worlds services. Content history
supplies entity timestamps and deployers; conversion status, fallback
selection and versions are native Registry facts. No external Registry cache or
Registry upstream URL supplies these responses.

`AB_REGISTRY_PG_CONNECTION_STRING` enables native moderation, conversion records,
queue state and embedded SQLx migrations. It does not require a second content
pool. `CONTENT_PG_CONNECTION_STRING` is optional for converter content indexing;
it does not select the Registry data source or enable its signed routes.
A failed configured Registry connection or migration aborts startup. Without native
persistence, Registry entity queries and writes return 501 rather than claiming
conversion state. The read-only denylist handler currently returns an empty collection when its
store is disabled; queue status returns 501.

`API_ADMIN_TOKEN` controls the presence of `/registry` and `/flush-cache`.
`DENYLIST_MODERATORS` supplies the startup moderator allowlist. Signed requests
validate timestamp freshness, authenticated metadata and the forwarded path's
match to the actual handler route. Signatures accept the service path or its
single `/asset-bundle-registry` alias after that binding check; EIP-1654 contract
wallet chains are unsupported.

See [native state and workload](REGISTRY-STATE.md) for persistence and migration
semantics.
