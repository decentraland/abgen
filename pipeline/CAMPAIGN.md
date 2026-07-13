# Campaign rendering — comparing the corpus at scale

`abgen-compare run` compares one entity set at a time. To render-compare thousands of entities
(whole cohorts of scenes / wearables / emotes) against upstream ab-cdn, the work fans across
native-GPU render nodes with a few pieces layered on the base pipeline. This is the operator's
view; machine-specific driver scripts (targets, keys, local paths) are kept out of this tree.

## Why a dedicated render node

The render stage is a real Unity editor rendering the client's shaders/URP, so the comparison
answers "does our bundle render like the actual client renders upstream's." That needs a
faithful GPU:

- **Windows / D3D11** or **macOS / Metal** render the DCL shaders natively.
- A headless Linux/Vulkan editor strips those shaders — meshes come out magenta — so it is
  fine for byte/structure analysis but **not** for the visual render.

So a render node is a box with a licensed Unity + a unity-explorer `Explorer` checkout and a
real desktop GPU session. `--platform windows` renders on the Windows node, `--platform mac`
on the macOS node; the two run concurrently to cover both platform axes.

## Local upstream mirror (kills the fetch bottleneck)

Fetching each upstream bundle from the public CDN is the throughput killer: round-trip latency
plus a politeness delay (`--abcdn-sleep`, default 0.5 s) dominates a batch. Holding a local copy
of the ab-cdn reference tree, served in ab-cdn URL layout with `--abcdn` pointed at it and
`--abcdn-sleep 0`, makes local fetches ~100–300× faster and un-throttled.

The reference tree is laid out per entity; a ~40-line stdlib `http.server` maps the two ab-cdn
URL shapes onto it:

- `GET /manifest/<entity>_<platform>.json` → `<shard>/<entity>/<platform>.manifest.json`
- `GET /<version>/<entity>/<file>`         → `<shard>/<entity>/<platform>/<file>`

Coverage is whatever the mirror snapshot holds; entities deployed after the snapshot 404 and
are skipped as unpairable (not fatal). Verify a served bundle is byte-identical to the public
CDN before trusting it.

## Reaching the servers from the render node

The render node needs three HTTP endpoints, typically bound to loopback on the analysis host:
the abgen JIT server ("ours"), the content server, and the local upstream mirror. Expose them to
the node over SSH reverse tunnels and point the run at `http://127.0.0.1:<port>` for each. Pin
the tunnels and URLs to `127.0.0.1` (not `localhost`) so a stray IPv6 listener on the same port
can't shadow them.

On a **Windows** node the pipeline runs under WSL2 driving the Windows `Unity.exe`
(`abgencompare/wsl.py` translates `/mnt/c/...` ↔ `C:\...` and stages render inputs to a
Windows-visible dir). Two WSL gotchas: set `networkingMode=mirrored` so the WSL shares the
Windows loopback (the tunnels land there), and register the Windows-interop binfmt if the
distro doesn't (`:WSLInterop:M::MZ::/init:PF` → `/proc/sys/fs/binfmt_misc/register`) or
`Unity.exe` fails with `Exec format error`. On a **macOS** node it's native — no translation,
no staging.

## Batch driver

The per-cohort operator driver is a small loop, resumable via a `.done` ledger:

1. `split -l <chunk>` the cohort's entity-id file into chunks (≈120–150 ids).
2. For each chunk not in `.done`, run `abgen-compare run --pointers-file <chunk> --slug
   <cohort>-<chunk> --platform <plat> --unity … --unity-project … --abgen-url … --content …
   --abcdn http://127.0.0.1:<mirror> --abcdn-sleep 0 --shader-bundle scene_ignore_<plat>` and
   append the chunk id on success.

One chunk = one Unity launch for all its entities (batch mode), so Unity boots once per chunk,
not per entity. A master driver chains the cohorts and each cohort keeps its own ledger, so the
whole campaign is interruptible and resumes where it left off.

## Building cohorts

Cohorts are plain entity-id lists (one per line) queried from the content deployments table
(`entity_type`, `entity_id`, `entity_metadata`, `entity_timestamp`, `deleter_deployment IS
NULL` = active). Examples:

- **recent deploys** — `ORDER BY entity_timestamp DESC LIMIT N`, optionally within a year window.
- **all of a type** — every active `emote`, base-avatar wearables (pointer `urn:…base-avatars:…`).
- **by complexity + all shapes** — group active wearables by `entity_metadata->'v'->'data'->>
  'category'` (the ~16 shapes) and, within each, spread the sample across the range of a size
  proxy (e.g. the main GLB's on-disk bytes) so both simple and heavy meshes are represented.

## Throughput

Per ~120-entity chunk on a 16-core Windows node, roughly:

| phase    | cost   | notes                                                    |
|----------|--------|----------------------------------------------------------|
| fetch    | ~90 s  | local mirror + `--abcdn-sleep 0`; the remote CDN was ~12 min |
| render   | ~4 min | one Unity session, jobs serial on the GPU — the floor    |
| classify | ~25 s  | pixel-diff fanned over all cores (was ~4 min single-threaded) |

≈ 6 min/chunk. The render is the floor; to go under it use the `--azimuths` / `--render-size`
dials (fewer/smaller shots trade angle/detail for speed) or add a second node. `skip byte-
identical from render` removes exact-byte matches from the GPU set for free — its payoff grows
as abgen converges toward byte-parity (recent cohorts still differ at the byte level even when
the render is imperceptible, so it currently skips little).
