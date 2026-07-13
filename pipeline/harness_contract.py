"""The contract between the compare pipeline and the Unity render harness.

This file is the single source of truth for how the pipeline's ``--render`` /
``--unity`` stage talks to the drop-in Unity Editor scripts in ``harness/``
(AbVisualCompare.cs, AbAnimCapture.cs, AbProjectSetup.cs). Both sides — this
module and the C# headers — state the same contract; when they disagree, fix
both in the same commit. The compare package (``compare/``) should import (or
vendor verbatim) this module rather than re-encoding any of these strings.

Stdlib-only, no side effects on import.

The contract, in one screen
===========================

Staging root (``AB_ROOT``, default ``/tmp/ab-compat``)::

    $AB_ROOT/
    ├── jobs.txt                        # or the file named by AB_JOBS
    ├── shader/scene_ignore_<platform>  # DCL/Scene shader bundle (or AB_SHADER path)
    ├── out/                            # harness writes everything here
    ├── harness.log                     # AbVisualCompare append log
    └── harness-anim.log                # AbAnimCapture append log

Jobs file — one job per line::

    <label>|<kind>|<abs bundle path>|<abs deps dir>     kind: glb | animated | texture
    # comments and blank lines are skipped; legacy 3-field lines = kind glb

Invocation (one Unity process per jobs file; batchmode, never -nographics)::

    Unity -batchmode -quit -projectPath <project> -executeMethod AbVisualCompare.Run -logFile <log>
    Unity -batchmode -quit -projectPath <project> -executeMethod AbAnimCapture.Run   -logFile <log>
    Unity -batchmode -quit -projectPath <project> -executeMethod AbProjectSetup.Apply -logFile <log>   # once

Environment knobs (all optional): see ``ENV`` below.

Outputs in ``$AB_ROOT/out/`` per job label:

    ============================  =============================================
    <label>-a<i>.png              AbVisualCompare, one per azimuth (glb/animated)
    <label>-anim.png              AbVisualCompare, animated only: all clips
                                  sampled at their own t=length/2, -a0 framing
    <label>-t<i>.png              AbVisualCompare, texture only: mip0 per Texture2D
    <label>.inventory.json        AbVisualCompare, always (even on failure)
    <label>.FAILED.txt            AbVisualCompare, exception text on job failure
    <label>-f<kk>.png             AbAnimCapture, kk = 00..frames-1 (animated only)
    <label>.anim.json             AbAnimCapture sidecar (clips, bounds, params)
    <label>.ANIMFAILED.txt        AbAnimCapture, exception text on job failure
    ============================  =============================================

Process exit code: 0 = run completed (per-job failures land in *.FAILED.txt —
always check for them), 2 = fatal (shader bundle missing, jobs file unreadable).
"""

from __future__ import annotations

import os
import shutil
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Sequence

PLATFORMS = ("mac", "windows", "linux", "webgl")
KINDS = ("glb", "animated", "texture")

ENV = {
    "AB_ROOT": "/tmp/ab-compat",
    "AB_JOBS": "jobs.txt",
    "AB_PLATFORM": None,
    "AB_SHADER": "shader/scene_ignore_{platform}",
    "AB_AZIMUTHS": "35,155,275",
    "AB_SIZE": "1024",
    "AB_FRAMES": "16",
    "AB_ANIM_SIZE": "512",
}

DEFAULT_AB_ROOT = ENV["AB_ROOT"]
DEFAULT_AZIMUTHS = (35.0, 155.0, 275.0)
DEFAULT_SIZE = 1024
DEFAULT_FRAMES = 16
DEFAULT_ANIM_SIZE = 512

SHADER_RELPATH = "shader/scene_ignore_{platform}"

HARNESS_SCRIPTS = ("AbProjectSetup.cs", "AbVisualCompare.cs", "AbAnimCapture.cs")

METHOD_SETUP = "AbProjectSetup.Apply"
METHOD_RENDER = "AbVisualCompare.Run"
METHOD_ANIM = "AbAnimCapture.Run"

HARNESS_DIR = Path(__file__).resolve().parent.parent / "harness"

@dataclass(frozen=True)
class Job:
    """One harness job. ``label`` is the output-name stem — by campaign
    convention ``<pair>-up`` / ``<pair>-ours`` so both sides of a pair land in
    the same out/ dir with distinct names. Paths must be absolute *on the
    render host* (remote staging remaps before writing the jobs file)."""

    label: str
    kind: str
    bundle: str
    deps_dir: str

    def line(self) -> str:
        for part, what in ((self.label, "label"), (self.kind, "kind"),
                           (self.bundle, "bundle"), (self.deps_dir, "deps_dir")):
            if "|" in part or "\n" in part:
                raise ValueError(f"job {what} contains a reserved character: {part!r}")
        if self.kind not in KINDS:
            raise ValueError(f"unknown kind {self.kind!r} (want one of {KINDS})")
        return f"{self.label}|{self.kind}|{self.bundle}|{self.deps_dir}"


def parse_job_line(line: str) -> Job | None:
    """Parse one jobs-file line; None for blanks/comments. Mirrors the C# parser
    (including the legacy 3-field = glb form)."""
    t = line.strip()
    if not t or t.startswith("#"):
        return None
    parts = t.split("|")
    if len(parts) >= 4:
        return Job(parts[0], parts[1], parts[2], parts[3])
    if len(parts) == 3:
        return Job(parts[0], "glb", parts[1], parts[2])
    raise ValueError(f"unparseable job line: {line!r}")


def write_jobs(path: str | Path, jobs: Iterable[Job]) -> int:
    """Write a jobs file; returns the number of jobs written."""
    jobs = list(jobs)
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    Path(path).write_text("".join(j.line() + "\n" for j in jobs), encoding="utf-8")
    return len(jobs)

def inventory_name(label: str) -> str:
    return f"{label}.inventory.json"


def failed_name(label: str) -> str:
    return f"{label}.FAILED.txt"


def anim_meta_name(label: str) -> str:
    return f"{label}.anim.json"


def anim_failed_name(label: str) -> str:
    return f"{label}.ANIMFAILED.txt"


def render_outputs(label: str, kind: str, n_azimuths: int = len(DEFAULT_AZIMUTHS)) -> list[str]:
    """Filenames AbVisualCompare produces for a *successful* job (texture jobs
    additionally produce ``<label>-t<i>.png`` per Texture2D — count unknown
    up front, glob for them)."""
    out = [f"{label}-a{i}.png" for i in range(n_azimuths)]
    if kind == "animated":
        out.append(f"{label}-anim.png")
    if kind == "texture":
        out = []
    out.append(inventory_name(label))
    return out


def anim_outputs(label: str, frames: int = DEFAULT_FRAMES) -> list[str]:
    """Filenames AbAnimCapture produces for a successful animated job."""
    return [f"{label}-f{k:02d}.png" for k in range(frames)] + [anim_meta_name(label)]


def harvest(out_dir: str | Path, labels: Sequence[str],
            kinds: dict[str, str] | None = None,
            n_azimuths: int = len(DEFAULT_AZIMUTHS)) -> dict[str, dict]:
    """Scan a harness out/ dir. Returns per label:
    ``{"pngs": [...], "inventory": path|None, "anim_meta": path|None,
       "failed": text|None, "anim_failed": text|None, "complete": bool}``.
    ``complete`` checks the still-render contract when ``kinds`` is given."""
    out_dir = Path(out_dir)
    result: dict[str, dict] = {}
    for label in labels:
        entry: dict = {"pngs": [], "inventory": None, "anim_meta": None,
                       "failed": None, "anim_failed": None, "complete": False}
        entry["pngs"] = sorted(str(p) for p in out_dir.glob(f"{label}-*.png"))
        inv = out_dir / inventory_name(label)
        if inv.exists():
            entry["inventory"] = str(inv)
        meta = out_dir / anim_meta_name(label)
        if meta.exists():
            entry["anim_meta"] = str(meta)
        fail = out_dir / failed_name(label)
        if fail.exists():
            entry["failed"] = fail.read_text(errors="replace")
        afail = out_dir / anim_failed_name(label)
        if afail.exists():
            entry["anim_failed"] = afail.read_text(errors="replace")
        if kinds and entry["failed"] is None:
            want = render_outputs(label, kinds[label], n_azimuths)
            have = {os.path.basename(p) for p in entry["pngs"]}
            if entry["inventory"]:
                have.add(inventory_name(label))
            entry["complete"] = all(w in have for w in want)
        result[label] = entry
    return result

def shader_relpath(platform: str) -> str:
    if platform not in PLATFORMS:
        raise ValueError(f"unknown platform {platform!r} (want one of {PLATFORMS})")
    return SHADER_RELPATH.format(platform=platform)


def stage_ab_root(ab_root: str | Path, platform: str,
                  shader_bundle: str | Path | None = None,
                  jobs: Iterable[Job] | None = None,
                  jobs_name: str = "jobs.txt") -> Path:
    """Create the AB_ROOT layout: shader/, out/, and (optionally) the shader
    bundle copy and a jobs file. Returns the AB_ROOT path."""
    root = Path(ab_root)
    (root / "out").mkdir(parents=True, exist_ok=True)
    (root / "shader").mkdir(parents=True, exist_ok=True)
    if shader_bundle is not None:
        shutil.copyfile(shader_bundle, root / shader_relpath(platform))
    if jobs is not None:
        write_jobs(root / jobs_name, jobs)
    return root

MARKER_PREFIX = "// abgen-harness "


def managed_text(src_text: str) -> str:
    """The exact file content an installed (managed) harness script has:
    marker header + verbatim source."""
    import hashlib
    sha = hashlib.sha256(src_text.encode("utf-8")).hexdigest()[:12]
    return (f"{MARKER_PREFIX}sha256:{sha} — installed by the abgen compare "
            f"pipeline (harness_contract.ensure_scripts); local edits will "
            f"be overwritten on the next render\n{src_text}")


def script_status(project_dir: str | Path,
                  harness_dir: str | Path = HARNESS_DIR,
                  scripts: Sequence[str] = HARNESS_SCRIPTS) -> dict[str, str]:
    """Read-only per-script state in <project>/Assets/Editor/:
    ``current | outdated | foreign | absent``. ``foreign`` = a file with our
    canonical name but no marker header (user/project copy)."""
    dest = Path(project_dir) / "Assets" / "Editor"
    out = {}
    for name in scripts:
        src = Path(harness_dir) / name
        if not src.exists():
            raise FileNotFoundError(f"harness script missing: {src}")
        want = managed_text(src.read_text(encoding="utf-8"))
        target = dest / name
        if not target.exists():
            out[name] = "absent"
            continue
        have = target.read_text(encoding="utf-8", errors="replace")
        if have == want:
            out[name] = "current"
        elif have.startswith(MARKER_PREFIX):
            out[name] = "outdated"
        else:
            out[name] = "foreign"
    return out


def ensure_scripts(project_dir: str | Path,
                   harness_dir: str | Path = HARNESS_DIR,
                   scripts: Sequence[str] = HARNESS_SCRIPTS,
                   log=None) -> dict[str, str]:
    """Auto-install/refresh the harness scripts in <project>/Assets/Editor/
    (idempotent; scripts stay installed between runs). Per-script action
    returned: ``installed | updated | kept | replaced-foreign``. Only files
    named exactly like ours are ever written; a same-named file WITHOUT our
    marker is overwritten with a warning (it would shadow the entry points
    anyway — Unity compiles by class name)."""
    dest = Path(project_dir) / "Assets" / "Editor"
    dest.mkdir(parents=True, exist_ok=True)
    status = script_status(project_dir, harness_dir, scripts)
    actions = {}
    for name in scripts:
        src_text = (Path(harness_dir) / name).read_text(encoding="utf-8")
        target = dest / name
        st = status[name]
        if st == "current":
            actions[name] = "kept"
            continue
        if st == "foreign" and log:
            log(f"harness: WARNING — {target} exists without the abgen "
                f"marker (user copy?); overwriting because the name/class "
                f"is exactly ours. Back it up if that was intentional.")
        target.write_text(managed_text(src_text), encoding="utf-8")
        actions[name] = {"absent": "installed", "outdated": "updated",
                         "foreign": "replaced-foreign"}[st]
    return actions

_VERSION_RE = r"\d+\.\d+\.\d+[abfp]\d+"


def editor_version_from_path(unity: str | None) -> str | None:
    """Best-effort editor version parsed from the binary path (Hub installs
    embed it: .../Hub/Editor/<version>/...). None when not inferable —
    callers should treat that as 'unknown', not as a mismatch."""
    import re
    if not unity:
        return None
    m = re.search(_VERSION_RE, str(unity))
    return m.group(0) if m else None


def project_editor_version(project_dir: str | Path | None) -> str | None:
    """m_EditorVersion from ProjectSettings/ProjectVersion.txt; None when the
    file is absent (our project-template deliberately ships without one —
    Unity stamps its own version on first open)."""
    if not project_dir:
        return None
    p = Path(project_dir) / "ProjectSettings" / "ProjectVersion.txt"
    try:
        for line in p.read_text(encoding="utf-8", errors="replace").splitlines():
            if line.startswith("m_EditorVersion:"):
                return line.split(":", 1)[1].strip()
    except OSError:
        return None
    return None

PROJECT_MODES = ("unity-explorer", "template", "custom", "none")


def project_mode(project_dir: str | Path | None) -> str:
    if not project_dir or not (Path(project_dir) / "Assets").is_dir():
        return "none"
    proj = Path(project_dir)
    if (proj / "Assets" / "DCL").is_dir():
        return "unity-explorer"
    manifest = proj / "Packages" / "manifest.json"
    deps = {}
    try:
        import json
        deps = json.loads(manifest.read_text(encoding="utf-8")).get("dependencies", {})
    except (OSError, ValueError):
        pass
    if any("dcl" in d or "decentraland" in d for d in deps):
        return "unity-explorer"
    template_manifest = HARNESS_DIR / "project-template" / "Packages" / "manifest.json"
    try:
        import json
        tdeps = json.loads(template_manifest.read_text(encoding="utf-8"))["dependencies"]
        if set(deps) == set(tdeps):
            return "template"
    except (OSError, ValueError, KeyError):
        pass
    return "custom"


def install_scripts(project_dir: str | Path, harness_dir: str | Path = HARNESS_DIR,
                    scripts: Sequence[str] = HARNESS_SCRIPTS) -> list[str]:
    """Deprecated shim — use :func:`ensure_scripts` (markered, idempotent,
    persistent). Kept so external callers of the old contract keep working."""
    ensure_scripts(project_dir, harness_dir, scripts)
    dest = Path(project_dir) / "Assets" / "Editor"
    return [str(dest / name) for name in scripts]

def unity_cmd(unity_binary: str, project_dir: str, log_file: str,
              method: str = METHOD_RENDER) -> list[str]:
    """argv for one harness invocation. Deliberately NO ``-nographics``: the
    harness needs a real GPU context (Metal/D3D11/Vulkan) to render; with
    -nographics Unity uses a null device and every capture comes out black."""
    return [
        unity_binary,
        "-batchmode", "-quit",
        "-projectPath", project_dir,
        "-executeMethod", method,
        "-logFile", log_file,
    ]


def harness_env(ab_root: str, platform: str, *,
                jobs_name: str | None = None,
                shader: str | None = None,
                azimuths: Sequence[float] | None = None,
                size: int | None = None,
                frames: int | None = None,
                anim_size: int | None = None) -> dict[str, str]:
    """Env-var dict for a harness invocation (merge over os.environ)."""
    env = {"AB_ROOT": str(ab_root), "AB_PLATFORM": platform}
    if jobs_name is not None:
        env["AB_JOBS"] = jobs_name
    if shader is not None:
        env["AB_SHADER"] = str(shader)
    if azimuths is not None:
        env["AB_AZIMUTHS"] = ",".join(f"{a:g}" for a in azimuths)
    if size is not None:
        env["AB_SIZE"] = str(size)
    if frames is not None:
        env["AB_FRAMES"] = str(frames)
    if anim_size is not None:
        env["AB_ANIM_SIZE"] = str(anim_size)
    return env


def windows_schtasks_cmds(task_name: str, bat_path: str) -> list[str]:
    """Windows render hosts driven over **remote ssh**: Unity started from an
    ssh session lands in a session with no GPU/desktop and renders black (or
    hangs on the graphics device). Run it in the interactive session via Task
    Scheduler instead — ``/IT`` (interactive token) + ``/RL HIGHEST``. The bat
    file should set the AB_* env vars and call the unity_cmd() line. Returns
    the commands to run over ssh/powershell (note PowerShell separates with
    ``;`` not ``&&``).

    This trap does NOT apply to interactive WSL2 shells on the same box: a
    Windows ``Unity.exe`` launched from interactive WSL inherits the logged-in
    desktop session (real GPU context). That path is first-class in the
    pipeline — see ``abgencompare/wsl.py`` and the README platform matrix."""
    return [
        f'schtasks /Create /F /TN {task_name} /TR "{bat_path}" /SC ONCE /ST 00:00 /RL HIGHEST /IT',
        f"schtasks /Run /TN {task_name}",
        f"schtasks /Query /TN {task_name} /V /FO LIST",
    ]
