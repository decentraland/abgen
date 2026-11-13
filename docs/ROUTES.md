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
| `GET /LOD/lods-unity/lods/{scene}_1.glb` | static object (far LOD, gltfpack layout) | written by the LOD lane; the abgen server has no fallback for it (no JIT, no read-through) |
| `GET /LOD/lods-bevy/lods/{scene}_1.glb` (+ `.glb.br`, `.stats.json`) | none | the Bevy far LOD copy (BC KTX2 atlases with mips), written beside the plain GLB by the LOD lane and by `abgen-lod bevy-far-lod`; no fallback on the abgen server, like the plain GLB; the `.br` object is stored with `Content-Encoding: br`, so a client fetches `{key}.br` by name; `public, max-age=31536000` like the plain GLB. See README, *Bevy far LOD copy* |
| `GET /bvwebgpu/{profile}/{entity}.pack` | browser pack object | served/generated as a range-friendly uncompressed object |
| `GET /bevy/manifest/{entity}.json` (prototype) | none | Bevy variants manifest; 404 until built (by the conversion lambda per entity, or offline with `abgen-corpus --bevy-variants`; nothing is built on request), and 404 `X-Abgen-Reason: bevy-variants-stale` while every copy on disk was built from an older generation; S3 read-through; `no-cache`. See README, *Bevy variants lane* |
| `GET /bevy/{prefix}/{name}` (prototype) | none | variant bytes (BC KTX2 with Zstandard levels, or GLB) at the `path` its manifest entry names (keys are named by the variant's inputs, not its CID); `immutable`, `Range`, CORS incl. `X-IPFS` preflight; S3 read-through; a `.br` sidecar is sent with `Content-Encoding: br` to clients that accept it |
| shader bundles (`scene_ignore_{windows,mac}`) | 404 (purged upstream) | self-primed from the vendored copies on first request |

Header semantics abgen implements natively: `ETag` with `If-None-Match` 304; `Range` 206/416 with
`Content-Range` (responses up to 8 MB buffered, larger streamed; `ABGEN_MAX_RANGE_BUFFER_BYTES`);
CORS `*` exposing `ETag`/`Content-Range`/`Accept-Ranges`/`Content-Length`.

Deltas an operator must know:

- Outside the Bevy variants lane, abgen does not generate, serve, or negotiate Brotli sidecars. Web deployments can apply HTTP compression at their own edge without changing abgen object keys. The Bevy lane's `glb-br` model variants (the original `.glb` plus a Brotli `.br` sidecar, on by default; `ABGEN_BEVY_MODELS=0` or `ABGEN_BEVY_PRECOMPRESS=none` turns them off) and its GLB variants' `.br` sidecars (when smaller than the variant; `ABGEN_BEVY_PRECOMPRESS=none` turns them off) are negotiated by `Accept-Encoding`. The Bevy far LOD copy's `.br` is a separate object, stored with `Content-Encoding: br` and fetched by name.
- Manifests are `no-cache` where upstream serves a cacheable TTL. A fronting CDN must forward
  `Range` and honor the `no-cache`.

## Registry (asset-bundle-registry)

| Route | Upstream registry | abgen |
|---|---|---|
| `GET /status` | health snapshot | not served - use `GET /health` |
| `POST /entities/active` (+`?world_name=`) | served | served; misses queue eager JIT builds; `world_name` resolves via `ABGEN_WORLDS_CONTENT_URL` |
| `POST /entities/versions` | served | served |
| `GET /entities/status/{id}` | served | served (content DB or catalyst proxy) |
| `GET /entities/status` (signed list) | served, signed-fetch | not served (404) |
| `GET /queues/status` | served (queue introspection) | not served (404) |
| `POST /profiles` | served | served (content DB or catalyst proxy) |
| `POST /profiles/metadata` | served | served (content DB or catalyst proxy) |
| `GET /worlds/{worldName}/manifest` | served | served (content DB or the worlds proxy lane) |
| `GET /denylist` | served | not served (404) |
| `POST/DELETE /denylist/{entityId}` (signed) | served | not served (404) |
| `POST /registry` (admin bearer) | served | not served (404) |
| `DELETE /flush-cache` (admin bearer) | served | not served (404) |

The not-served rows are the registry's signed/write/ops surface - they belong to a catalyst or a
dedicated registry deployment, not this converter. Ops routes abgen adds instead: `/ping`,
`/health`, `/livez`, `/readyz`, `/metrics` (bearer-gated when `ABGEN_METRICS_BEARER_TOKEN` is set).

`GET /health` carries two fields describing the running binary rather than its corpus: `version`
(the crate version, matching the `abgen-v{version}-{target}` release artifact) and `pid` (this
process's OS id). They exist for loopback supervision - a client that finds a server already bound
to the port it wants can tell whether that server is the release it pins, and can signal it if not.

On the index routes (`POST /entities/active|versions`), a connected content DB supplies real
timestamps and deployer identity; without one, the built-in fallback serves the same shapes with
`timestamp: 0` and an empty deployer.

## Registry data source: content DB, catalyst proxy fallback

The four unsigned registry routes (`/profiles`, `/profiles/metadata`,
`/entities/status/{id}`, `/worlds/{world_name}/manifest`) are always mounted and pick their
entity source at boot:

- **content DB** - build with feature `content-db` and set `CONTENT_PG_CONNECTION_STRING` (or
  the `POSTGRES_*` parts): entities resolve from a catalyst content Postgres with real
  timestamps and deployer identity.
- **catalyst proxy** - otherwise the routes proxy the catalyst at `ABGEN_CATALYST_URL`: one
  10-second upstream attempt per request; pointer and id lookups union into
  `POST {ABGEN_CATALYST_URL}/entities/active` calls, deduped keeping the newest entity;
  `/profiles*` filters to `type == "profile"`. An upstream 404 stays a 404; an upstream
  transport failure returns 502. World manifests resolve through the SSRF-guarded worlds lane
  and need `ABGEN_WORLDS_CONTENT_URL` enabled (default: the public worlds server; disabled means
  empty world results). Proxy mode serves an empty deployer.

In both modes the standalone server mounts an open world policy (empty denylist) - the
`WorldPolicy` trait in `crate/dcl-contents` is the seam for a deployment that supplies one.
`GET /health` reports the active mode: `content-db` or `catalyst-proxy`.
