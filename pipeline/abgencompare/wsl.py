"""WSL2 <-> Windows bridging for the Unity render stage.

The headless pipeline is plain x86_64-linux and needs nothing from this
module. The render stage, however, drives a Unity *Editor* — and on a
Windows box the supported shape is: pipeline + abgen build under WSL2,
Unity installed and licensed on the Windows side. WSL's binfmt interop can
launch the Windows ``Unity.exe`` directly from an interactive WSL shell
(it inherits the logged-in desktop session, so the GPU context is real —
the schtasks dance in ``harness_contract.windows_schtasks_cmds`` is only
for *remote ssh* driving), but every path the Windows process reads must
be spelled the Windows way. This module is that translation layer:

- :func:`is_wsl` — running inside WSL? (``/proc/version`` mentions
  "microsoft"; WSL1 says "Microsoft", WSL2 "microsoft-standard-WSL2").
- :func:`is_windows_unity` — is the ``--unity`` argument a Windows-side
  editor (under a ``/mnt/<drive>/`` automount or a ``*.exe``)?
- :func:`to_windows` — ``wslpath -w`` (``/mnt/c/x`` -> ``C:\\x``, WSL-fs
  paths -> ``\\\\wsl.localhost\\<distro>\\...``). Overridable for tests via
  ``$ABGEN_WSLPATH`` (point it at a fake wslpath executable).
- :func:`is_windows_visible` — cheap-for-Windows path? Everything under
  ``/mnt/<drive>/`` is a native Windows drive; WSL-fs paths are reachable
  from Windows only through the slow ``\\\\wsl.localhost`` share, and a
  repo checkout normally lives on the WSL fs (that is where builds are
  fast). The render stage therefore *stages* its inputs to a Windows-
  visible dir (``--win-staging``, default ``/mnt/c/abgen-runs/<run-id>``)
  when the run dir is not already under ``/mnt``.

Stdlib-only, no side effects on import (same rules as harness_contract).
"""

from __future__ import annotations

import os
import string
import subprocess

DEFAULT_WIN_STAGING_ROOT = "/mnt/c/abgen-runs"

WSLPATH_ENV = "ABGEN_WSLPATH"


def is_wsl(proc_version: str = "/proc/version") -> bool:
    """True when running inside WSL (1 or 2)."""
    try:
        with open(proc_version, encoding="utf-8", errors="replace") as f:
            return "microsoft" in f.read().lower()
    except OSError:
        return False


def is_windows_path(path: str | None) -> bool:
    """Windows-spelled path? (``C:\\...`` drive-absolute or ``\\\\unc\\...``)."""
    if not path:
        return False
    if path.startswith("\\\\"):
        return True
    return (len(path) >= 3 and path[0] in string.ascii_letters
            and path[1] == ":" and path[2] in ("\\", "/"))


def is_windows_visible(path: str) -> bool:
    """Under a ``/mnt/<drive>/`` automount (a native Windows drive)?"""
    parts = os.path.abspath(path).split("/")
    return (len(parts) >= 3 and parts[1] == "mnt"
            and len(parts[2]) == 1 and parts[2].lower() in string.ascii_lowercase)


def is_windows_unity(unity: str | None) -> bool:
    """Is the ``--unity`` argument a Windows-side editor binary?"""
    if not unity:
        return False
    if is_windows_path(unity):
        return True
    return unity.lower().endswith(".exe") or is_windows_visible(unity)


def _wslpath(flag: str, path: str) -> str:
    exe = os.environ.get(WSLPATH_ENV, "wslpath")
    r = subprocess.run([exe, flag, str(path)], capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(
            f"{exe} {flag} {path!r} failed rc={r.returncode}: {r.stderr.strip()}")
    return r.stdout.strip()


def to_windows(path: str) -> str:
    """WSL path -> Windows spelling (``wslpath -w``). The path should exist —
    older wslpath builds refuse to translate nonexistent paths."""
    return _wslpath("-w", path)


def to_wsl(path: str) -> str:
    """Windows spelling -> WSL path (``wslpath -u``)."""
    return _wslpath("-u", path)


def normalize_input(path: str | None) -> str | None:
    """Accept user-supplied CLI paths in either spelling: under WSL a
    Windows-spelled path is converted so the pipeline's own os.path checks
    (isfile/isdir) work; everything else passes through untouched."""
    if path and is_wsl() and is_windows_path(path):
        return to_wsl(path)
    return path


def default_win_staging(run_id: str) -> str:
    return os.path.join(DEFAULT_WIN_STAGING_ROOT, run_id)


def add_wslenv(env: dict, names) -> dict:
    """WSL forwards ONLY the variables named in ``WSLENV`` to Windows
    processes it launches — without this the harness would see none of the
    AB_* knobs. Values are passed verbatim (no ``/p`` flag: the caller
    already translated path values to Windows spellings). Preserves any
    existing WSLENV entries (which may carry ``/flags`` suffixes)."""
    existing = [e for e in env.get("WSLENV", "").split(":") if e]
    present = {e.split("/", 1)[0] for e in existing}
    for n in names:
        if n not in present:
            existing.append(n)
    env["WSLENV"] = ":".join(existing)
    return env


def run_windows_unity(cmd, env, log_path_wsl, log, timeout=3600, poll=2.0,
                      report_every=30.0):
    """Launch a Windows Unity .exe from WSL (binfmt interop starts it in the
    interactive session, so it gets the real GPU/desktop context) and poll
    the -logFile through its WSL-visible twin while it runs. Returns the
    exit code; raises ``subprocess.TimeoutExpired`` on timeout (process
    killed first). ``env`` must already carry the WSLENV passthrough (see
    :func:`add_wslenv`)."""
    import time

    p = subprocess.Popen(cmd, env=env)
    t0 = time.time()
    next_report = t0 + report_every
    last_size = -1
    while True:
        rc = p.poll()
        if rc is not None:
            return rc
        now = time.time()
        if now - t0 > timeout:
            p.kill()
            p.wait()
            raise subprocess.TimeoutExpired(cmd, timeout)
        if now >= next_report:
            next_report = now + report_every
            try:
                size = os.path.getsize(log_path_wsl)
            except OSError:
                size = -1
            if size != last_size:
                log(f"render: unity running ({int(now - t0)}s, "
                    f"log {size} bytes)")
                last_size = size
        time.sleep(poll)
