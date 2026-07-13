# pipeline/ - `abgen-compare`

Generalized AB-parity run pipeline (python3, stdlib-only). One run = one entity set x one
platform x two bundle sources (local abgen JIT server "ours" vs upstream ab-cdn), analyzed
headlessly and optionally rendered.

## CLI

```
abgen-compare run (--pointer <x,y | urn | entityId> | --pointers-file FILE)
                  [--platform mac|windows|linux|webgl]      # default mac
                  [--content URL]     # default https://peer.decentraland.org/content
                  [--abcdn URL]       # default https://ab-cdn.decentraland.org
                  [--abcdn-sleep SEC] # per-bundle politeness delay (default 0.5;
                                      #   set 0 for a LOCAL upstream mirror)
                  [--unity /path/to/Unity --unity-project <Explorer>
                   [--shader-bundle scene_ignore_<plat>]
                   [--azimuths 35,155,275]   # render angles; fewer = faster
                   [--render-size 1024]      # square px; smaller = faster
                   [--win-staging DIR]]                     # enables render stage
                  [--abgen-url URL]   # reuse a running abgen server (default
                                      # probes 127.0.0.1:5147, else spawns one)
                  [--tag TAG]... [--description TEXT] [--slug NAME] [--runs-root DIR]
abgen-compare list  [--runs-root DIR] [--json]
abgen-compare serve [port] [--runs-root DIR]   # execs site/server.py (multi-run)
abgen-compare config [show | set <key> <value> | unset <key>] [--user]
```

**Batch mode.** `--pointers-file FILE` (one pointer/entity-id per line, `#` comments ok)
resolves EVERY entity into ONE run and ONE Unity launch — the render harness loads/renders/
unloads each bundle in a single editor session, amortizing per-entity Unity boot cost. Requires
`--slug`; unresolvable pointers are skipped, not fatal. How the corpus cohorts are rendered at
scale (see [CAMPAIGN.md](CAMPAIGN.md)).

Runs root: `--runs-root` > `$ABGEN_RUNS_DIR` > `<repo>/runs/`.

## Render inputs: two Unity inputs + persistent config

The render stage needs the editor binary and a host project for the harness scripts; both
resolve through a persistent tool config (`abgencompare/toolconfig.py`) shared by `run`,
`serve` and the wizard. Precedence, highest wins:

1. CLI flags `--unity` / `--unity-project` / `--win-staging`
2. env `ABGEN_UNITY_BINARY` / `ABGEN_UNITY_PROJECT` / `ABGEN_WIN_STAGING`
3. `<repo>/abgen-compare.json` (per checkout; gitignored)
4. `~/.config/abgen-compare/config.json` (`abgen-compare config set ... --user`)

Host-project modes (recorded as render provenance in the run's `config.json`, stamped on
rev-2 rows' `prov`, shown in the site header):

- **unity-explorer** - a `decentraland/unity-explorer` checkout (`--unity-project
  .../Explorer`): client-faithful, recommended; needs its private UPM deps resolvable - see
  `harness/README.md`.
- **template** - a copy of `harness/project-template/`: self-contained,
  comparative-but-NOT-client-exact.
- **custom** - any other URP project (must be URP + linear color space).

Harness scripts auto-install into `<project>/Assets/Editor/` at render time (marker-managed,
idempotent; details + overwrite caveat in `harness/README.md`). The editor version (parsed from
a Hub-style path) is cross-checked against `ProjectSettings/ProjectVersion.txt`; mismatch warns
but doesn't block.

### Windows / WSL2

With a Windows-side editor (`--unity` under `/mnt/<drive>/` or `*.exe`, either path spelling
accepted): paths the Windows process reads translate via `wslpath -w`, `AB_*` knobs forward
through `WSLENV`, the `.exe` launches directly (interactive WSL inherits the desktop session =
real GPU), log polled via the WSL-visible path. Run dir not under `/mnt` -> render inputs stage
to `--win-staging` (default `/mnt/c/abgen-runs/<run-id>`), harness logs copied back. Self-test:
`python3 pipeline/selftest_wsl.py` (fake `wslpath` via `$ABGEN_WSLPATH`).

## What `run` does

1. **resolve** - pointer (`x,y` parcel / urn) via `POST {content}/entities/active`, or entity
   id via `GET {content}/contents/{id}` -> `entity-meta.jsonl`.
2. **ours** - probes `--abgen-url` (default `127.0.0.1:5147`); if unhealthy, spawns
   `target/release/abgen` with a scratch out_root in the run dir (`ours-out/`, `ours-cache/`,
   `ABGEN_ROOT=<repo>` for templates, TURBOJPEG auto-discovered), terminated after the fetch.
   `GET /manifest/{entity}_{platform}.json` triggers the JIT build; every bundle payload is
   fetched over HTTP and archived in `ours/`.
3. **upstream** - same manifest/payload walk against ab-cdn (`--abcdn-sleep` spacing, default
   0.5 s; set 0 against a local mirror, fetch-and-keep in `upstream/`; 404 payloads recorded
   as purged name-only pairs).
4. **pair** - match key = `lowercase(fileCID)`; name forms `<cid>_<plat>` (v12-v48),
   `<cid>_<32hex>_<plat>` (v49), bare `<cid>` (vintage webgl); kind from the entity content
   map (glb/gltf -> `glb`, or `animated` for emotes; images -> `texture`).
5. **headless analysis (always)** - byte diff (sizes, sha256, first-diff, differing ranges);
   structure diff: `objdump` both sides, `pid=<int>` and `cab-<32hex>` normalized to fixed
   sentinels (`abgencompare/analyze.py`), multiset line comparison (object order follows
   nondeterministic path-ids), AssetBundle-manifest + `metadata.json` records pinned as
   expected diffs (timestamp/version); `texcmp` (per-texture fmt/dims/mips + mip0 RGBA pixel
   stats) + `texpng` (mip0 PNGs -> `tex-images/<pair>-{up,ours}.png`).
6. **render (only with `--unity`)** - stages `ab-compat/` per `harness_contract.py`
   (Windows-visible + `wslpath`-translated under WSL), ensures `harness/*.cs` in the project, ONE
   batchmode invocation (`--azimuths`/`--render-size` forwarded as `AB_AZIMUTHS`/`AB_SIZE`),
   harvests `out/` -> `renders/`, classifies with the campaign comparator, appends rev-2 matrix
   rows (newest-wins; `prov` = host-project mode + editor version). Two throughput cuts:
   **byte-identical pairs drop from the Unity job set** (a byte-match renders identically, so its
   row emits directly as `identical`), and the pixel-classify loop **fans over a process pool**
   (`os.cpu_count()` workers; a broken pool falls back to serial). The harness force-terminates
   after flushing outputs to avoid Unity's `-quit` thread-abort hang.
7. **site data** - `site-data.json` in the canonical schema (sides `up`/`ours`, `tags[]`,
   run-relative shot paths) served by `site/server.py` as `/r/<run-id>/data.json`.

A finished run gets a `COMPLETE` marker and is never touched again - re-runs create new run
dirs.

## Classification

- Six labels: `identical / imperceptible / visible / stub / structural / fail`.
- Render comparator: differing pixel = RGB int16 abs-diff, per-pixel max over channels, >0;
  `ppm = max-over-azimuths(px)*1e6/(W*H)` (actual shot dims, fallback 1024^2);
  `identical` = 0 px, `imperceptible` <= 200 ppm, `visible` > 200 ppm.
- Amnesty: `visible` glb/animated with delta>8 ppm <=200 relabels `imperceptible` (carries an
  `amp-amnesty` note).
- Amplitude bands per azimuth: `[px_all, px>8, px>32, max delta, mean delta]`; `dim-mismatch`
  sentinel; single-shot reuse = trivially identical.
- Textures: `corpusStub` -> `stub`; `identical-decode` -> `identical`; `visible` +
  `(no pixel compare)` -> `structural`; `loadFail*/skipped/behaveDiff` -> `fail`.
- Headless glb/animated (no renders): byte-identical -> `identical`;
  normalized-structure-identical -> `imperceptible` (note says "no render compare");
  structure drift -> `structural`; dump failure -> `fail`.

## Run directory

See the docstring in `abgencompare/runmodel.py` for the full tree (config.json,
entity-meta.jsonl, manifests/, ours/, upstream/, renders/, tex-images/,
analysis/{pairs,bytediff,structure,texcmp,matrix}.jsonl, site-data.json, run.log, COMPLETE).
