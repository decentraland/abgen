#!/usr/bin/env python3
"""Self-test for the WSL2 render bridge (abgencompare/wsl.py + the render
stage's staging/translation). Needs NO Unity, NO Windows, NO WSL: the real
``wslpath`` is replaced by a fake via the ``$ABGEN_WSLPATH`` override, so the
translation logic runs anywhere python does.

    python3 pipeline/selftest_wsl.py          # or: python3 -m unittest ...
"""
import os
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import harness_contract as hc  # noqa: E402
from abgencompare import render, wsl  # noqa: E402

FAKE_WSLPATH = r'''#!/usr/bin/env python3
import sys
flag, path = sys.argv[1], sys.argv[2]
def to_w(p):
    parts = p.split("/")
    if len(parts) >= 3 and parts[1] == "mnt" and len(parts[2]) == 1:
        return parts[2].upper() + ":\\" + "\\".join(parts[3:])
    return "\\\\wsl.localhost\\Fake" + p.replace("/", "\\")
def to_u(p):
    if len(p) >= 3 and p[1] == ":":
        return "/mnt/" + p[0].lower() + "/" + p[3:].replace("\\", "/")
    return p.replace("\\", "/")
print(to_w(path) if flag == "-w" else to_u(path))
'''


class WslBridgeTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="abgen-wsl-selftest-")
        self.addCleanup(self.tmp.cleanup)
        fake = os.path.join(self.tmp.name, "fake-wslpath")
        with open(fake, "w") as f:
            f.write(FAKE_WSLPATH)
        os.chmod(fake, 0o755)
        self.env_patch = mock.patch.dict(os.environ, {wsl.WSLPATH_ENV: fake})
        self.env_patch.start()
        self.addCleanup(self.env_patch.stop)

    def test_is_wsl_reads_proc_version(self):
        p = os.path.join(self.tmp.name, "proc-version")
        with open(p, "w") as f:
            f.write("Linux version 5.15.167.4-microsoft-standard-WSL2 ...\n")
        self.assertTrue(wsl.is_wsl(p))
        with open(p, "w") as f:
            f.write("Linux version 6.18.33 (nixbld@localhost) ...\n")
        self.assertFalse(wsl.is_wsl(p))
        self.assertFalse(wsl.is_wsl(os.path.join(self.tmp.name, "missing")))

    def test_is_windows_path(self):
        self.assertTrue(wsl.is_windows_path(r"C:\Unity\Editor\Unity.exe"))
        self.assertTrue(wsl.is_windows_path(r"\\wsl.localhost\Ubuntu\home"))
        self.assertFalse(wsl.is_windows_path("/mnt/c/Unity/Unity.exe"))
        self.assertFalse(wsl.is_windows_path(None))
        self.assertFalse(wsl.is_windows_path(""))

    def test_is_windows_visible(self):
        self.assertTrue(wsl.is_windows_visible("/mnt/c/abgen-runs/x"))
        self.assertTrue(wsl.is_windows_visible("/mnt/d"))
        self.assertFalse(wsl.is_windows_visible("/home/user/abgen/runs/x"))
        self.assertFalse(wsl.is_windows_visible("/mnt/wsl/something"))

    def test_is_windows_unity(self):
        self.assertTrue(wsl.is_windows_unity(
            "/mnt/c/Program Files/Unity/Hub/Editor/6000.0.23f1/Editor/Unity.exe"))
        self.assertTrue(wsl.is_windows_unity(r"C:\Unity\Editor\Unity.exe"))
        self.assertTrue(wsl.is_windows_unity("Unity.EXE"))
        self.assertFalse(wsl.is_windows_unity("/usr/bin/unity-editor"))
        self.assertFalse(wsl.is_windows_unity(None))

    def test_to_windows_drive_and_wslfs(self):
        self.assertEqual(wsl.to_windows("/mnt/c/abgen-runs/r1/jobs.txt"),
                         r"C:\abgen-runs\r1\jobs.txt")
        self.assertEqual(wsl.to_windows("/home/u/abgen/runs/r1"),
                         r"\\wsl.localhost\Fake\home\u\abgen\runs\r1")

    def test_to_wsl(self):
        self.assertEqual(wsl.to_wsl(r"C:\abgen-runs\r1"), "/mnt/c/abgen-runs/r1")

    def test_add_wslenv(self):
        env = {"WSLENV": "USERPROFILE/p:AB_ROOT"}
        wsl.add_wslenv(env, ["AB_ROOT", "AB_PLATFORM", "AB_JOBS"])
        self.assertEqual(env["WSLENV"],
                         "USERPROFILE/p:AB_ROOT:AB_PLATFORM:AB_JOBS")
        env = {}
        wsl.add_wslenv(env, ["AB_ROOT"])
        self.assertEqual(env["WSLENV"], "AB_ROOT")

    def _mk_run_dir(self):
        run_dir = os.path.join(self.tmp.name, "runs", "20260704-000000-t")
        for d in ("jobs", "renders",
                  "ours/ent1/windows", "upstream/ent1/windows"):
            os.makedirs(os.path.join(run_dir, d))
        pairs = []
        for cid in ("bafybundle1", "bafybundle2"):
            for side in ("ours", "upstream"):
                with open(os.path.join(run_dir, side, "ent1", "windows",
                                       f"{cid}_windows"), "wb") as f:
                    f.write(side.encode() + cid.encode())
            pairs.append({
                "pair_id": f"p-{cid}", "kind": "glb", "entity": "ent1",
                "cid": cid, "status": "paired",
                "ours_path": f"ours/ent1/windows/{cid}_windows",
                "upstream_path": f"upstream/ent1/windows/{cid}_windows",
            })
        return run_dir, pairs

    def test_stage_jobs_wsl_staging_and_translation(self):
        run_dir, pairs = self._mk_run_dir()
        staging = os.path.join(self.tmp.name, "win-staging")
        logs = []
        base, ab_root, jobs, kinds = render._stage_jobs(
            run_dir, pairs, wsl_mode=True, win_staging=staging,
            log=logs.append)
        self.assertEqual(base, staging)
        self.assertEqual(ab_root, os.path.join(staging, "ab-compat"))
        for side in ("ours", "upstream"):
            for cid in ("bafybundle1", "bafybundle2"):
                staged = os.path.join(staging, side, "ent1", "windows",
                                      f"{cid}_windows")
                self.assertTrue(os.path.isfile(staged), staged)
        self.assertEqual(len(jobs), 4)
        want_prefix = r"\\wsl.localhost\Fake"
        for j in jobs:
            self.assertTrue(j.bundle.startswith(want_prefix), j.bundle)
            self.assertTrue(j.deps_dir.startswith(want_prefix), j.deps_dir)
            self.assertNotIn("/", j.bundle)
        self.assertEqual(kinds, {f"p-{c}-{s}": "glb"
                                 for c in ("bafybundle1", "bafybundle2")
                                 for s in ("up", "ours")})
        jobs_file = os.path.join(self.tmp.name, "jobs.txt")
        hc.write_jobs(jobs_file, jobs)
        with open(jobs_file) as f:
            parsed = [hc.parse_job_line(l) for l in f]
        self.assertEqual(parsed, jobs)
        self.assertTrue(any("staging render inputs" in m for m in logs))

    def test_stage_jobs_windows_visible_run_dir_stays_in_place(self):
        run_dir, pairs = self._mk_run_dir()
        with mock.patch.object(wsl, "is_windows_visible", return_value=True):
            base, ab_root, jobs, _ = render._stage_jobs(
                run_dir, pairs, wsl_mode=True, win_staging=None,
                log=lambda m: None)
        self.assertEqual(base, run_dir)
        for j in jobs:
            self.assertTrue(wsl.is_windows_path(j.bundle), j.bundle)

    def test_stage_jobs_non_wsl_unchanged(self):
        run_dir, pairs = self._mk_run_dir()
        base, ab_root, jobs, _ = render._stage_jobs(
            run_dir, pairs, wsl_mode=False, win_staging=None,
            log=lambda m: None)
        self.assertEqual(base, run_dir)
        self.assertEqual(ab_root, os.path.join(run_dir, "ab-compat"))
        for j in jobs:
            self.assertTrue(j.bundle.startswith(run_dir), j.bundle)

    def test_unity_cmd_windows_spelling(self):
        proj = os.path.join(self.tmp.name, "proj")
        logf = os.path.join(self.tmp.name, "unity.log")
        os.makedirs(proj)
        open(logf, "w").close()
        cmd = hc.unity_cmd("/mnt/c/Unity/Editor/Unity.exe",
                           wsl.to_windows(proj), wsl.to_windows(logf))
        self.assertEqual(cmd[0], "/mnt/c/Unity/Editor/Unity.exe")
        self.assertIn(r"\\wsl.localhost\Fake", cmd[cmd.index("-projectPath") + 1])
        self.assertIn(r"\\wsl.localhost\Fake", cmd[cmd.index("-logFile") + 1])

    def test_normalize_input_translates_only_under_wsl(self):
        with mock.patch.object(wsl, "is_wsl", return_value=True):
            self.assertEqual(wsl.normalize_input(r"C:\proj"), "/mnt/c/proj")
            self.assertEqual(wsl.normalize_input("/home/u/proj"), "/home/u/proj")
        with mock.patch.object(wsl, "is_wsl", return_value=False):
            self.assertEqual(wsl.normalize_input(r"C:\proj"), r"C:\proj")

    def test_run_windows_unity_returns_rc_and_polls_log(self):
        logf = os.path.join(self.tmp.name, "u.log")
        code = (f"open({logf!r}, 'w').write('unity says hi')\n"
                "import time; time.sleep(0.3)\n"
                "raise SystemExit(3)")
        msgs = []
        rc = wsl.run_windows_unity([sys.executable, "-c", code],
                                   dict(os.environ), logf, msgs.append,
                                   timeout=30, poll=0.05, report_every=0.1)
        self.assertEqual(rc, 3)
        self.assertTrue(any("unity running" in m for m in msgs), msgs)

    def test_run_windows_unity_timeout_kills(self):
        with self.assertRaises(subprocess.TimeoutExpired):
            wsl.run_windows_unity(
                [sys.executable, "-c", "import time; time.sleep(60)"],
                dict(os.environ), os.path.join(self.tmp.name, "nolog"),
                lambda m: None, timeout=0.5, poll=0.05, report_every=10)


if __name__ == "__main__":
    unittest.main(verbosity=2)
