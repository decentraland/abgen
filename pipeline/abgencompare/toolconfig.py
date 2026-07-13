"""Persistent tool configuration shared by run / serve / the setup wizard.

The render stage needs TWO Unity inputs — the editor binary AND a Unity
project to host the harness — plus (under WSL) an optional Windows-visible
staging dir. Re-typing them per run invites drift between the CLI and the
wizard, so they live in a small config file both sides read.

Keys (all optional):

    unity_editor    Unity editor binary (the --unity flag)
    unity_project   harness host project dir (the --unity-project flag)
    win_staging     WSL only: Windows-visible render staging dir

Value precedence, highest wins (documented in the README):

    1. CLI flags            --unity / --unity-project / --win-staging
    2. environment          ABGEN_UNITY_BINARY / ABGEN_UNITY_PROJECT /
                            ABGEN_WIN_STAGING
    3. repo config          <repo>/abgen-compare.json      (per checkout;
                            gitignored — machine-local paths never commit)
    4. user config          $XDG_CONFIG_HOME/abgen-compare/config.json
                            (default ~/.config/abgen-compare/config.json)

``abgen-compare config`` prints the effective values + their sources;
``abgen-compare config set|unset <key> [<value>] [--user]`` edits the repo
(default) or user file. Stdlib-only, no side effects on import.
"""

from __future__ import annotations

import json
import os

KEYS = ("unity_editor", "unity_project", "win_staging")

ENV_MAP = {
    "unity_editor": "ABGEN_UNITY_BINARY",
    "unity_project": "ABGEN_UNITY_PROJECT",
    "win_staging": "ABGEN_WIN_STAGING",
}

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
REPO_CONFIG_NAME = "abgen-compare.json"


def repo_config_path(repo_root: str | None = None) -> str:
    return os.path.join(repo_root or REPO_ROOT, REPO_CONFIG_NAME)


def user_config_path() -> str:
    base = os.environ.get("XDG_CONFIG_HOME") or os.path.expanduser("~/.config")
    return os.path.join(base, "abgen-compare", "config.json")


def load_file(path: str) -> dict:
    """Known keys from one config file; {} when missing. A malformed file
    raises — silently ignoring a config the user wrote hides real mistakes."""
    try:
        with open(path, encoding="utf-8") as f:
            data = json.load(f)
    except FileNotFoundError:
        return {}
    if not isinstance(data, dict):
        raise ValueError(f"{path}: expected a JSON object, got {type(data).__name__}")
    unknown = sorted(set(data) - set(KEYS))
    if unknown:
        raise ValueError(f"{path}: unknown keys {unknown} (known: {list(KEYS)})")
    return {k: data[k] for k in KEYS if data.get(k)}


def effective(cli: dict | None = None, *, repo_root: str | None = None,
              env: dict | None = None) -> tuple[dict, dict]:
    """Resolve every key through the precedence chain.

    ``cli`` maps config keys to the parsed flag values (None = not given).
    Returns ``(values, sources)`` — both keyed by KEYS; source is one of
    ``cli | env | repo (<path>) | user (<path>) | unset``."""
    env = os.environ if env is None else env
    cli = cli or {}
    repo_p, user_p = repo_config_path(repo_root), user_config_path()
    repo_cfg, user_cfg = load_file(repo_p), load_file(user_p)
    values, sources = {}, {}
    for k in KEYS:
        if cli.get(k):
            values[k], sources[k] = cli[k], "cli"
        elif env.get(ENV_MAP[k]):
            values[k], sources[k] = env[ENV_MAP[k]], f"env {ENV_MAP[k]}"
        elif repo_cfg.get(k):
            values[k], sources[k] = repo_cfg[k], f"repo ({repo_p})"
        elif user_cfg.get(k):
            values[k], sources[k] = user_cfg[k], f"user ({user_p})"
        else:
            values[k], sources[k] = None, "unset"
    return values, sources


def save(updates: dict, scope: str = "repo", *, repo_root: str | None = None) -> str:
    """Merge ``updates`` (value None = remove key) into the repo or user
    config file; returns the path written."""
    if scope not in ("repo", "user"):
        raise ValueError(f"scope must be repo|user, got {scope!r}")
    unknown = sorted(set(updates) - set(KEYS))
    if unknown:
        raise ValueError(f"unknown config keys {unknown} (known: {list(KEYS)})")
    path = repo_config_path(repo_root) if scope == "repo" else user_config_path()
    current = load_file(path)
    for k, v in updates.items():
        if v is None:
            current.pop(k, None)
        else:
            current[k] = v
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(current, f, indent=1, sort_keys=True)
        f.write("\n")
    os.replace(tmp, path)
    return path
