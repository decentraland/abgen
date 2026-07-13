# Render harness - Unity capture scripts for pixel-level parity

Optional render stage: three Unity Editor scripts + a minimal project template turn any Unity
6000.x install into a deterministic screenshot rig for asset bundles (byte/structural verdicts
never need Unity). Machine-readable half:
[`../pipeline/harness_contract.py`](../pipeline/harness_contract.py) (generates the jobs file,
stages `AB_ROOT`, invokes Unity) - change both sides in the same commit.

| Script | Entry point | Does |
|---|---|---|
| `AbProjectSetup.cs` | `AbProjectSetup.Apply` | one-shot: URP pipeline asset + linear color space + MSAA off |
| `AbVisualCompare.cs` | `AbVisualCompare.Run` | still renders (N azimuths), texture mip0 blits, per-bundle `inventory.json` |
| `AbAnimCapture.cs` | `AbAnimCapture.Run` | animation frame series (`-f00..`) for GIF assembly + motion-parity metrics |

## 1. One-time setup

### 1.1 Unity install + license

Any Unity 6000.x editor; batchmode needs an activated license (Unity Hub sign-in once, Personal
is free, suffices). Headless: `Unity -batchmode -createManualActivationFile -quit` -> upload the
`.alf` at <https://license.unity3d.com/manual> -> `Unity -batchmode -manualLicenseFile <file>.ulf
-quit`. License errors: instant exit with `[Licensing]` lines in `-logFile`.

### 1.2 The project

Set via `--unity-project` or `abgen-compare config set unity_project ...`; the mode is recorded in
run provenance (`config.json` -> site header).

**Mode 1 - `decentraland/unity-explorer` checkout (recommended, client-faithful).** Point
`unity_project` at the `Explorer/` subdir: renders in the real client's URP/shader environment,
the only mode that answers "does it look right in the client." Skip `AbProjectSetup` (already
URP + linear). First open resolves UPM deps (git on PATH; eight packages come from the private
`decentraland/unity-explorer-packages` repo - needs Decentraland org access via ssh key or
https+PAT `insteadOf` rewrite), taking tens of minutes once. Apply the shipped patch; drift = 3
files: `Explorer/Packages/manifest.json` and `packages-lock.json` (1-line ssh->https rewrite each
for the public `unity-shared-dependencies`), `docs/ABGEN-COMPARE.md` (new). Don't use
vendored/drifted forks - renders are only client-faithful when the checkout matches upstream.

```bash
git clone https://github.com/decentraland/unity-explorer.git
cd unity-explorer
git checkout 94365208b356287656b74f59a1905de5c135f716   # tested; later dev OK
git apply /path/to/abgen/harness/unity-explorer-abgen.patch  # or git am
abgen-compare config set unity_project "$PWD/Explorer"
```

**Mode 2 - the minimal template in this repo (self-contained fallback).**
Comparative-but-NOT-client-exact; labeled `template` in run provenance.

```bash
cp -r harness/project-template /path/to/ab-harness-project
mkdir -p /path/to/ab-harness-project/Assets/Editor
cp harness/AbProjectSetup.cs /path/to/ab-harness-project/Assets/Editor/
"$UNITY" -batchmode -quit -projectPath /path/to/ab-harness-project \
         -executeMethod AbProjectSetup.Apply -logFile /tmp/absetup.log
```

(`AbProjectSetup.cs` is copied by hand: this step runs before any render; only render time
auto-installs scripts.) `AbProjectSetup.Apply` creates `Assets/Settings/AbUrp.asset` + renderer,
sets it for all quality levels, switches the player to linear color space; exit 0 + an
`ABSETUP: pipeline=... OK` log line = done. The template ships no `ProjectVersion.txt` and no
serialized URP assets (Unity stamps its own version, URP config created programmatically), so it
works across all 6000.x releases.

Both modes: the pipeline auto-installs the capture scripts into `<project>/Assets/Editor/` at
render time; each carries a `// abgen-harness sha256:...` marker (re-runs no-op until this repo's
copy changes). A same-named script without the marker is overwritten with a warning - back up
local edits. Manual `cp harness/*.cs .../Assets/Editor/` works; the next render re-stamps. Don't
mix: both sides of a pair must render from the same project on the same GPU.

Settings the scripts assume:

- URP active - the DCL `scene_ignore_*` shader bundle is URP-compiled; under built-in every
  material falls back to `Hidden/InternalErrorShader` (magenta), inventories report
  `errorShader > 0`. `AbVisualCompare` logs `pipeline=<name>` at start; `builtin` = stop and fix.
- Linear color space - gamma poisons every pixel metric.
- Determinism - MSAA off, shadows off (per-shot), flat ambient; the same job on the same machine
  must be pixel-stable across runs.

### 1.3 The shader bundle

Every generated bundle's `metadata.json` references the compiled `DCL/Scene` shader as a
`dcl/scene_ignore_<platform>` dependency; the harness pre-loads it from
`$AB_ROOT/shader/scene_ignore_<platform>`, skipping `dcl/` deps during per-bundle loading. Flavor
must match the render platform: material references point at the shader bundle's per-platform
CAB name (`crate/src/cabname.rs`: windows `CAB-51fbd4c9...`, mac `CAB-5ba4993b...`); a
wrong-platform donor loads fine and resolves nothing - magenta on both sides.

Where ab-cdn ships it (the vintage gotcha: a wrong-era shader is a *different* foreign donor, same
failure as above):

- v38-v44-era manifests: literal `dcl` entry in `files`, payloads under it:
  `https://ab-cdn.decentraland.org/<ver>/<entity>/dcl/scene_ignore_<platform>` - real
  per-platform builds (the bare `.../<ver>/<entity>/dcl` URL 404s);
- <=v36-era: one flat `scene_ignore_windows` shared across platforms (byte-identical on mac
  manifests; only correct for bundles of that same vintage);
- v49-era: no shader listed; the old root URL
  `https://ab-cdn.decentraland.org/dcl/scene_ignore_windows` 404s.

Preference order:

1. `--shader-bundle` (explicit override);
2. `crate/shader/scene_ignore_<platform>` - vendored, sha-pinned (windows flavor only; the
   converter's harvest donor);
3. entity-shipped (`abgencompare/fetch.py::stage_upstream_shader`): the
   `dcl/scene_ignore_<platform>` (or flat-era `scene_ignore_*`) from the run's own upstream
   manifests -> `<run>/shader/` (how mac renders get a CAB-correct shader);
4. none -> render stage skipped with guidance (headless verdicts stand).

Before any big run, render 1-2 probe pairs and require `errorShader: 0` in both inventories;
prefer the dominant shader variant shipped alongside the entities being compared.

## 2. Per-run contract

### 2.1 `AB_ROOT` staging layout

```
$AB_ROOT/                          (default /tmp/ab-compat)
|-- jobs.txt                       # or the file named by AB_JOBS
|-- shader/scene_ignore_<plat>     # 1.3
|-- out/                           # all outputs land here
|-- harness.log                    # AbVisualCompare, appended across runs
`-- harness-anim.log               # AbAnimCapture, appended across runs
```

### 2.2 `jobs.txt`

One job per line; `#` comments and blank lines skipped:

```
<label>|<kind>|<abs bundle path>|<abs deps dir>
```

- `kind` in `glb` | `animated` | `texture` (legacy 3-field lines = `glb`).
- `label` = output-name stem; convention `<pair>-up` / `<pair>-ours`.
- Paths absolute on the render host (the pipeline remaps when staging to a remote box).
- Dep resolution: `metadata.json` in the bundle -> recursive load from the deps dir (tries
  `<dep>`, `<dep>_<platform>`, then lowercase match - vintage `Qm...` case drift); no
  `metadata.json` -> every sibling bundle in the deps dir loads (vintage upstream tolerance).
- `AbAnimCapture` reads the same file, processes only `kind=animated` lines; one staging serves
  both scripts.

### 2.3 Environment knobs

| Var | Default | Meaning |
|---|---|---|
| `AB_ROOT` | `/tmp/ab-compat` | staging root |
| `AB_JOBS` | `jobs.txt` | jobs file (relative to `AB_ROOT` unless absolute) - lets the pipeline chunk without rewriting `jobs.txt` |
| `AB_PLATFORM` | editor OS (`mac`/`windows`/`linux`) | bundle platform: shader default + `_<platform>` dep suffix |
| `AB_SHADER` | `shader/scene_ignore_<AB_PLATFORM>` | shader bundle override (relative to `AB_ROOT` unless absolute) |
| `AB_AZIMUTHS` | `35,155,275` | camera azimuths (deg); N values -> `-a0..-a(N-1)`; `AbAnimCapture` uses the first only |
| `AB_SIZE` | `1024` | still-render size (px, square) |
| `AB_FRAMES` | `16` | frames per animated job (`AbAnimCapture`) |
| `AB_ANIM_SIZE` | `512` | frame-series render size |

Defaults reproduce the parity campaign (3 azimuths, elevation 28 deg, FOV 50, 2x-bounds-radius
distance, 1024^2 stills, 16x512^2 frames); classifier thresholds (<=200 ppm etc.) were calibrated
at those settings - change both sides at once or not at all.

### 2.4 Invocation

```bash
export AB_ROOT=/tmp/ab-compat AB_PLATFORM=mac
"$UNITY" -batchmode -quit -projectPath <project> -executeMethod AbVisualCompare.Run -logFile /tmp/abvis.log
"$UNITY" -batchmode -quit -projectPath <project> -executeMethod AbAnimCapture.Run   -logFile /tmp/abanim.log
```

One Unity process per jobs file. Never pass `-nographics` (null graphics device, every capture
black); a real GPU context is required - see section 3. Exit codes: `0` = completed (job
failures land as `<label>.FAILED.txt` / `<label>.ANIMFAILED.txt` - always sweep for them),
`2` = fatal (shader bundle or jobs file unusable).

### 2.5 Outputs (`$AB_ROOT/out/`)

| File | From | What |
|---|---|---|
| `<label>-a<i>.png` | AbVisualCompare | one still per azimuth (glb/animated) |
| `<label>-anim.png` | AbVisualCompare | animated only: every clip (name-sorted) sampled at its own `t=length/2`, `-a0` framing - emote bundles carry an invisible `_Avatar` clip plus prop clips, sampling just one leaves meshes at rest |
| `<label>-t<i>.png` | AbVisualCompare | texture only: mip0 blit per `Texture2D`, native size, sRGB |
| `<label>.inventory.json` | AbVisualCompare | always written, even on failure - see below |
| `<label>.FAILED.txt` | AbVisualCompare | exception text on job failure |
| `<label>-f<kk>.png` | AbAnimCapture | frame `kk`, sampled at `t = clip.length*k/N` (loop-friendly; wrap frame not repeated) |
| `<label>.anim.json` | AbAnimCapture | `{label, frames, size, azimuth, clips:[{name,length}], boundsRest, boundsUnion}` |
| `<label>.ANIMFAILED.txt` | AbAnimCapture | exception text on job failure |

`inventory.json` fields (the site's inventory-diff contract): `label, kind, bundle,
depPath(metadata|same-dir-scan|none), mainAsset, deps, assetCount, assetTypeCounts,
assets[{name,type}], gameObjectAssets, instantiated, renderers, skinnedRenderers, materials,
errorShader, meshAssets, vertexTotal, bounds{center,extents},
animationClips[{name,length,curves,objectRefCurves,totalCurves,statSize,frameRate,legacy,looping,
humanMotion,wrapMode}], textures[{name,width,height,format,mips}], texPngs, sampledClips, error`.
(`totalCurves` uses `GetAnimationClipStats` via reflection - `GetCurveBindings` returns `[]` for
bundle-loaded clips.)

### 2.6 Frame-series capture (AbAnimCapture) - deliberate, don't "optimize"

Per frame: fresh-instantiate -> sample every clip once at `t_k` -> render -> destroy
(`AnimationClip.SampleAnimation` only writes the properties the clip animates at that time;
re-sampling a live instance keeps stale values from earlier samples). Framing = rest bounds
unioned with all sampled bounds (same fresh-instantiate discipline), then one fixed camera for
the whole series - motion never exits the frame, framing identical on both sides. The pipeline
assembles `-f<kk>.png` into per-side GIFs and computes the motion-parity verdicts (per-frame MAE,
motion-energy profiles) host-side.

## 3. Platform notes

- macOS - Metal renders fine headless over ssh; never `-nographics`.
- Windows over remote ssh - no GPU/desktop: renders black or hangs creating the graphics device.
  Launch in the interactive session via Task Scheduler:
  `schtasks /Create /F /TN abvis /TR "C:\dcl\run-abvis.bat" /SC ONCE /ST 00:00 /RL HIGHEST /IT`
  then `schtasks /Run /TN abvis` (a user must be logged into the console session; `/IT` =
  interactive token, `/RL HIGHEST` = elevation). The `.bat` sets the `AB_*` vars and runs the 2.4
  command line. PowerShell separates commands with `;`, not `&&`. Helper:
  `harness_contract.windows_schtasks_cmds()`.
- Windows from interactive WSL2 - no dance needed: a Windows `Unity.exe` launched from an
  interactive WSL shell inherits the logged-in desktop session; the pipeline drives this natively
  (path translation, WSLENV, Windows-visible staging) - see `pipeline/abgencompare/wsl.py`.
- Linux - needs a working Vulkan/GL device; on a headless box run under a virtual display
  (`Xvfb`/`sway --headless`) with real GPU drivers. Software GL renders subtly differently - don't
  compare its output against a hardware run.
- webgl bundles - no desktop editor loads them; webgl runs stay at byte/structural verdicts
  (pipeline handles this).

## 4. What you must bring

1. A Unity 6000.x install + activated license (1.1) - not shipped, not downloadable.
2. A GPU - thresholds assume hardware rendering; both sides on the same machine/driver.
3. A platform-matched shader bundle for non-windows platforms (1.3; the repo vendors windows
   only; the pipeline auto-stages from upstream fetches when the vintage ships one).
4. Windows: an interactively logged-in session for `schtasks /IT`.
