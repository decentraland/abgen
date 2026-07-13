"""Optional render stage: drive the Unity Editor harness (harness/
AbVisualCompare.cs) on both sides of every renderable pair, harvest renders +
inventories into renders/, and classify (campaign classify-chunk.py inventory
semantics + the compare-classes.py pixel comparator + Δ>8 amnesty).

All harness-contract strings (env knobs, jobs-file format, output names,
invocation argv) come from pipeline/harness_contract.py — the single source of
truth shared with the C# scripts. Never re-encode them here.
"""
import json
import os
import shutil
import subprocess
import sys

from . import IMPERCEPTIBLE_PPM
from . import wsl
from .classify import render_class

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import harness_contract as hc  # noqa: E402


def render_provenance(unity, project):
    """Provenance of the render environment, recorded into the run's
    config.json and surfaced on the site: which project MODE hosted the
    harness (unity-explorer checkout = client-faithful; template =
    comparative-but-not-client-exact), plus paths/versions."""
    mode = hc.project_mode(project)
    editor_v = hc.editor_version_from_path(unity)
    project_v = hc.project_editor_version(project)
    return {
        "project_mode": mode,
        "unity_project": project,
        "editor_version": editor_v,
        "project_editor_version": project_v,
        "summary": f"{mode} project · editor {editor_v or 'unknown'}",
    }


def check_render_stack(unity, project, shader_bundle, platform, log):
    problems = []
    exe_ok = (unity and os.path.isfile(unity)
              and (os.access(unity, os.X_OK)
                   or (wsl.is_wsl() and wsl.is_windows_unity(unity))))
    if not exe_ok:
        problems.append(f"unity binary not executable: {unity}")
    if not (project and os.path.isdir(os.path.join(project or "", "Assets"))):
        problems.append(
            f"unity project has no Assets/: {project} "
            f"(a starting point ships at harness/project-template/; the "
            f"client-faithful host is a decentraland/unity-explorer checkout)"
        )
    if not (shader_bundle and os.path.isfile(shader_bundle)):
        problems.append(
            f"shader bundle missing: {shader_bundle} "
            f"(need scene_ignore_{platform}; the repo vendors scene_ignore_windows "
            f"at crate/shader/)"
        )
    if not problems:
        prov = render_provenance(unity, project)
        log(f"render: project mode = {prov['project_mode']} "
            + ("(client-faithful shader/URP environment)"
               if prov["project_mode"] == "unity-explorer"
               else "(comparative baseline, not client-exact)"))
        ev, pv = prov["editor_version"], prov["project_editor_version"]
        if ev and pv and ev != pv:
            log(f"render: WARNING — editor {ev} vs project ProjectVersion "
                f"{pv}; Unity will migrate/reimport (slow first run) and "
                f"renders may drift from campaign baselines")
    for p in problems:
        log(f"render: PRECONDITION FAIL — {p}")
    return not problems


def _stage_jobs(run_dir, pairs, wsl_mode, win_staging, log):
    """Pick the staging base and build the harness job list.

    Normally everything stays inside the run dir. Under WSL with a
    Windows-side Unity, the Windows process reads every path in Windows
    spelling — and if the run dir lives on the WSL filesystem (where a
    repo checkout belongs, for build speed) it is only reachable from
    Windows over the slow \\\\wsl.localhost share, so the render inputs are
    first copied to a Windows-visible staging dir (``--win-staging``,
    default ``/mnt/c/abgen-runs/<run-id>``). Returns
    ``(base, ab_root, jobs, kinds)``; job paths are Windows-spelled when
    ``wsl_mode``."""
    base = run_dir
    if wsl_mode and not wsl.is_windows_visible(run_dir):
        base = win_staging or wsl.default_win_staging(os.path.basename(run_dir))
        os.makedirs(base, exist_ok=True)
        log(f"render: WSL — run dir not Windows-visible, staging render "
            f"inputs at {base}")
    ab_root = os.path.join(base, "ab-compat")
    jobs, kinds, staged = [], {}, set()
    for r in pairs:
        kind = r["kind"] if r["kind"] in hc.KINDS else "glb"
        for side, rel in (("up", r["upstream_path"]), ("ours", r["ours_path"])):
            if base != run_dir:
                deps_rel = os.path.dirname(rel)
                if deps_rel not in staged:
                    shutil.copytree(os.path.join(run_dir, deps_rel),
                                    os.path.join(base, deps_rel),
                                    dirs_exist_ok=True)
                    staged.add(deps_rel)
            bundle = os.path.join(base, rel)
            deps_dir = os.path.dirname(bundle)
            if wsl_mode:
                bundle = wsl.to_windows(bundle)
                deps_dir = wsl.to_windows(deps_dir)
            label = f"{r['pair_id']}-{side}"
            jobs.append(hc.Job(label, kind, bundle, deps_dir))
            kinds[label] = kind
    return base, ab_root, jobs, kinds


def run_harness(run_dir, pairs, platform, unity, project, shader_bundle, log,
                timeout=3600, win_staging=None, azimuths=None, size=None):
    """Stage AB_ROOT (in the run dir, or Windows-visible under WSL), run
    Unity batchmode once, harvest out/ into renders/ (labels are already
    canonical <pair>-{up,ours}). ``azimuths``/``size`` (when given) flow to the
    harness as AB_AZIMUTHS / AB_SIZE; None leaves the harness defaults."""
    wsl_mode = wsl.is_wsl() and wsl.is_windows_unity(unity)
    base, ab_root, jobs, kinds = _stage_jobs(run_dir, pairs, wsl_mode,
                                             win_staging, log)
    hc.stage_ab_root(ab_root, platform, shader_bundle=shader_bundle, jobs=jobs)
    shutil.copyfile(os.path.join(ab_root, "jobs.txt"),
                    os.path.join(run_dir, "jobs", "jobs.txt"))
    log(f"render: {len(jobs)} harness jobs staged at {ab_root}"
        + (" (wsl mode: Windows-spelled job paths)" if wsl_mode else ""))

    actions = hc.ensure_scripts(project, log=log)
    log("render: harness scripts in {}/Assets/Editor: {}".format(
        project, ", ".join(f"{k}={v}" for k, v in actions.items())))
    unity_log = os.path.join(base, "jobs", "unity.log")
    os.makedirs(os.path.dirname(unity_log), exist_ok=True)
    env = dict(os.environ)
    if wsl_mode:
        open(unity_log, "ab").close()
        cmd = hc.unity_cmd(unity, wsl.to_windows(project),
                           wsl.to_windows(unity_log))
        overlay = hc.harness_env(wsl.to_windows(ab_root), platform,
                                 azimuths=azimuths, size=size)
        env.update(overlay)
        wsl.add_wslenv(env, overlay)
    else:
        cmd = hc.unity_cmd(unity, project, unity_log)
        env.update(hc.harness_env(ab_root, platform, azimuths=azimuths, size=size))
    log(f"render: {' '.join(cmd)}")
    try:
        if wsl_mode:
            rc = wsl.run_windows_unity(cmd, env, unity_log, log,
                                       timeout=timeout)
        else:
            rc = subprocess.run(cmd, env=env, timeout=timeout).returncode
        log(f"render: unity exited rc={rc}"
            + (" (fatal: shader/jobs unreadable)" if rc == 2 else ""))
    finally:
        if base != run_dir:
            for src, name in ((unity_log, "unity.log"),
                              (os.path.join(ab_root, "harness.log"),
                               "harness.log"),
                              (os.path.join(ab_root, "harness-anim.log"),
                               "harness-anim.log")):
                if os.path.exists(src):
                    shutil.copyfile(src,
                                    os.path.join(run_dir, "jobs", name))

    out_dir = os.path.join(ab_root, "out")
    result = hc.harvest(out_dir, sorted(kinds), kinds=kinds)
    n = 0
    if os.path.isdir(out_dir):
        for fname in sorted(os.listdir(out_dir)):
            shutil.copy2(os.path.join(out_dir, fname),
                         os.path.join(run_dir, "renders", fname))
            n += 1
    incomplete = [l for l, e in result.items() if not e["complete"] and not e["failed"]]
    failed = [l for l, e in result.items() if e["failed"]]
    log(f"render: harvested {n} files into renders/ "
        f"({len(failed)} FAILED, {len(incomplete)} incomplete)")
    return result

def _load_inv(p):
    try:
        return json.load(open(p))
    except (OSError, ValueError):
        return None


def _norm_assets(inv):
    return sorted(
        (a["name"], a["type"])
        for a in inv.get("assets", [])
        if not (a["type"] == "TextAsset" and a["name"] == "metadata.json")
    )


def _tcounts(assets):
    d = {}
    for _n, t in assets:
        d[t] = d.get(t, 0) + 1
    return d


def _clip_key(c):
    return (c["name"], round(c["length"], 3), c.get("totalCurves"), c["legacy"],
            c["looping"], round(c.get("frameRate", 0), 3))


def _bclose(a, b, tol=1e-3):
    if a is None and b is None:
        return True
    if a is None or b is None:
        return False
    return all(abs(x - y) <= tol
               for k in ("center", "extents")
               for x, y in zip(a[k], b[k]))


def inventory_diff(io_, iu):
    diff = {}
    ao, au = _norm_assets(io_), _norm_assets(iu)
    tco, tcu = _tcounts(ao), _tcounts(au)
    if ao != au:
        if tco != tcu:
            diff["assetTypeCounts"] = {"ours": tco, "up": tcu}
        else:
            diff["assetNames"] = {
                "oursOnly": [list(a) for a in ao if a not in au][:6],
                "upOnly": [list(a) for a in au if a not in ao][:6],
            }
    for k in ("gameObjectAssets", "instantiated", "renderers", "skinnedRenderers",
              "materials", "errorShader", "meshAssets", "vertexTotal"):
        if io_.get(k) != iu.get(k):
            diff[k] = {"ours": io_.get(k), "up": iu.get(k)}
    if not _bclose(io_.get("bounds"), iu.get("bounds")):
        diff["bounds"] = {"ours": io_.get("bounds"), "up": iu.get("bounds")}
    co = sorted(_clip_key(c) for c in io_.get("animationClips", []))
    cu = sorted(_clip_key(c) for c in iu.get("animationClips", []))
    if co != cu:
        diff["clips"] = {"ours": co, "up": cu}
    return diff


def classify_rendered_pair(run_dir, pair_id, kind, n_azimuths=3):
    """-> (class, label, ppm, ppm8, maxd, px_per_angle, inventory_diff, notes,
    amp) — comparator + inventory semantics of the campaign classifier.

    Pure + picklable (top-level): safe to fan out over a process pool.
    ``n_azimuths`` matches the harness AB_AZIMUTHS count (default 3)."""
    ev = os.path.join(run_dir, "renders")
    p = lambda side, suf: os.path.join(ev, f"{pair_id}-{side}{suf}")  # noqa: E731
    io_ = _load_inv(p("ours", ".inventory.json"))
    iu = _load_inv(p("up", ".inventory.json"))
    failed_o = os.path.exists(p("ours", ".FAILED.txt"))
    failed_u = os.path.exists(p("up", ".FAILED.txt"))
    notes = []
    if (io_ is None and not failed_o) or (iu is None and not failed_u):
        return ("skipped", "fail", None, None, None, None, {},
                ["missing inventory (harness incomplete?)"], None)
    diff = inventory_diff(io_, iu) if (io_ and iu) else {}

    suffixes = [f"-a{i}" for i in range(n_azimuths)] + (["-anim"] if kind == "animated" else [])
    up_shots, ours_shots, missing_o, missing_u = [], [], [], []
    for s in suffixes:
        po, pu = p("ours", s + ".png"), p("up", s + ".png")
        eo, eu = os.path.exists(po), os.path.exists(pu)
        if eo and eu:
            up_shots.append(pu)
            ours_shots.append(po)
        elif eo != eu:
            (missing_u if eo else missing_o).append(s)

    if failed_o and failed_u:
        same = io_ and iu and io_.get("error") == iu.get("error") and not diff
        cls = "identical" if same else "loadFailOurs"
        notes.append("both sides FAILED: ours=%r up=%r"
                     % ((io_ or {}).get("error"), (iu or {}).get("error")))
        return (cls, "identical" if cls == "identical" else "fail",
                None, None, None, None, diff, notes, None)
    if failed_o:
        return ("loadFailOurs", "fail", None, None, None, None, diff,
                ["ours FAILED: %s" % ((io_ or {}).get("error") or "")], None)
    if failed_u:
        return ("loadFailUpstream", "fail", None, None, None, None, diff,
                ["upstream FAILED: %s" % ((iu or {}).get("error") or "")], None)
    if missing_o or missing_u:
        return ("visible", "visible", None, None, None, None, diff,
                [f"render presence mismatch ours-missing {missing_o} "
                 f"up-missing {missing_u}"], None)

    anim_types = ("AnimationClip", "AnimatorController", "Animation", "Animator")
    tco = _tcounts(_norm_assets(io_)) if io_ else {}
    tcu = _tcounts(_norm_assets(iu)) if iu else {}
    if ("clips" in diff) or any(tco.get(t, 0) != tcu.get(t, 0) for t in anim_types):
        bd = (["clips"] if "clips" in diff else []) + (
            ["animTypeCount"] if any(tco.get(t, 0) != tcu.get(t, 0) for t in anim_types) else [])
        return ("behaveDiff", "fail", None, None, None, None, diff,
                ["anim inventory diff: " + ",".join(bd)], None)

    from .metrics import compare_shots

    amp = compare_shots(up_shots, ours_shots)
    cls, label, ppm, ppm8, maxd, pxa, cnotes = render_class(amp, kind)
    notes.extend(cnotes)

    core_keys = ("gameObjectAssets", "instantiated", "renderers",
                 "skinnedRenderers", "materials", "errorShader", "bounds")
    core_ok = not any(k in diff for k in core_keys)
    if cls == "identical" and diff:
        cls = label = "imperceptible" if core_ok else "visible"
        notes.append(("format-level" if core_ok else "core")
                     + " inventory diff: " + ",".join(diff.keys()))
    elif cls == "imperceptible" and not core_ok:
        cls = label = "visible"
        notes.append("core inventory diff: " + ",".join(k for k in core_keys if k in diff))
    elif cls == "visible" and ppm is not None and ppm > IMPERCEPTIBLE_PPM:
        notes.append(f"maxPpm={ppm:.0f}")
    return cls, label, ppm, ppm8, maxd, pxa, diff, notes, amp
