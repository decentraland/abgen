"""Offline unit tests for the simval harness additions (dirserve routing,
id_diff/signature_of math, struct pinned accounting, verdict metric gating).

  cd pipeline && python3 -m unittest test_simval -v
"""
import json
import os
import shutil
import sys
import tempfile
import threading
import unittest
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from abgencompare import dirserve, verdict  # noqa: E402
from abgencompare.analyze import (id_diff, signature_of,  # noqa: E402
                                  struct_diff_from_texts, vol_norm)
from abgencompare.fetch import norm_bundle_name  # noqa: E402

BAFK = "bafkreieb6izdbhadi6vyjniq3hhpb363i44rf676wpjygyjrlhzsfp7eoa"
QM = "QmUYY3YkLmQFTDvqeqRz36yDTdUQjvWMrUzLQCqQg4H2xj"


class TestDirservePure(unittest.TestCase):
    def test_shard_of(self):
        self.assertEqual(dirserve.shard_of(BAFK), "b6")
        self.assertEqual(dirserve.shard_of(QM), "QmUY")

    def test_split_manifest_name(self):
        self.assertEqual(dirserve.split_manifest_name(f"{BAFK}_windows"),
                         (BAFK, "windows"))
        self.assertEqual(dirserve.split_manifest_name(f"{QM}_mac"), (QM, "mac"))
        self.assertEqual(dirserve.split_manifest_name(BAFK), (BAFK, "webgl"))

    def test_platform_of_file(self):
        self.assertEqual(dirserve.platform_of_file("cid_windows"), "windows")
        self.assertEqual(dirserve.platform_of_file("cid_" + "a" * 32 + "_mac"),
                         "mac")
        self.assertEqual(dirserve.platform_of_file("dcl/scene_ignore_windows"),
                         "windows")
        self.assertIsNone(dirserve.platform_of_file("cid_unknown"))

    def test_parse_drops(self):
        self.assertEqual(dirserve.parse_drops([f"{BAFK}/x_windows"]),
                         {(BAFK, "x_windows")})
        self.assertEqual(dirserve.parse_drops([f"{BAFK}/dcl/scene_ignore_mac"]),
                         {(BAFK, "dcl/scene_ignore_mac")})
        with self.assertRaises(SystemExit):
            dirserve.parse_drops(["no-slash"])


class TestDirserveTree(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="dirserve-test-")
        edir = os.path.join(self.tmp, "ref", "b6", BAFK)
        os.makedirs(os.path.join(edir, "windows"))
        self.man = {"version": "v36", "files": ["aaa_windows", "bbb_windows"],
                    "exitCode": 0, "date": "2025-01-01T00:00:00.000Z"}
        self.raw = json.dumps(self.man, indent=2).encode()
        with open(os.path.join(edir, "windows.manifest.json"), "wb") as f:
            f.write(self.raw)
        for n in ("aaa_windows", "bbb_windows"):
            with open(os.path.join(edir, "windows", n), "wb") as f:
                f.write(b"payload-" + n.encode())
        fdir = os.path.join(self.tmp, "flat", BAFK)
        os.makedirs(fdir)
        for n in ("ccc_windows", "ddd_windows", "eee_mac"):
            with open(os.path.join(fdir, n), "wb") as f:
                f.write(b"flat-" + n.encode())
        self.ref = os.path.join(self.tmp, "ref")
        self.flat = os.path.join(self.tmp, "flat")

    def tearDown(self):
        shutil.rmtree(self.tmp)

    def test_ref_manifest_verbatim_without_drops(self):
        body = dirserve.manifest_response(self.ref, "ref", f"{BAFK}_windows",
                                          set())
        self.assertEqual(body, self.raw)

    def test_ref_manifest_drop_filters(self):
        body = dirserve.manifest_response(self.ref, "ref", f"{BAFK}_windows",
                                          {(BAFK, "aaa_windows")})
        self.assertEqual(json.loads(body)["files"], ["bbb_windows"])

    def test_ref_manifest_missing(self):
        self.assertIsNone(dirserve.manifest_response(self.ref, "ref",
                                                     f"{BAFK}_mac", set()))

    def test_flat_manifest_synthesis(self):
        body = dirserve.manifest_response(self.flat, "flat", f"{BAFK}_windows",
                                          set())
        man = json.loads(body)
        self.assertEqual(man["files"], ["ccc_windows", "ddd_windows"])
        self.assertEqual(man["exitCode"], 0)
        self.assertEqual(man["version"], "local-detguid")
        mac = json.loads(dirserve.manifest_response(self.flat, "flat",
                                                    f"{BAFK}_mac", set()))
        self.assertEqual(mac["files"], ["eee_mac"])

    def test_payload_paths(self):
        p = dirserve.payload_path(self.ref, "ref", BAFK, "aaa_windows", set())
        self.assertTrue(p and p.endswith(os.path.join("windows", "aaa_windows")))
        self.assertIsNone(dirserve.payload_path(
            self.ref, "ref", BAFK, "aaa_windows", {(BAFK, "aaa_windows")}))
        self.assertIsNone(dirserve.payload_path(self.ref, "ref", BAFK,
                                                "../escape_windows", set()))
        p = dirserve.payload_path(self.flat, "flat", BAFK, "ccc_windows", set())
        self.assertTrue(p and os.path.isfile(p))

    def test_http_roundtrip(self):
        srv = dirserve.make_server(self.ref, "ref", 0,
                                   {(BAFK, "bbb_windows")})
        t = threading.Thread(target=srv.serve_forever, daemon=True)
        t.start()
        base = f"http://127.0.0.1:{srv.server_address[1]}"
        try:
            man = json.loads(urllib.request.urlopen(
                f"{base}/manifest/{BAFK}_windows.json").read())
            self.assertEqual(man["files"], ["aaa_windows"])
            body = urllib.request.urlopen(
                f"{base}/v36/{BAFK}/aaa_windows").read()
            self.assertEqual(body, b"payload-aaa_windows")
            with self.assertRaises(urllib.error.HTTPError) as cm:
                urllib.request.urlopen(f"{base}/v36/{BAFK}/bbb_windows")
            self.assertEqual(cm.exception.code, 404)
        finally:
            srv.shutdown()
            srv.server_close()


class TestAnalyzeMath(unittest.TestCase):
    def test_signature_of_collapses_digits(self):
        a = signature_of("class=43 pid=※  name=mesh vtx=120")
        b = signature_of("class=43 pid=※ name=mesh   vtx=99999")
        self.assertEqual(a, b)
        self.assertEqual(a, "class=# pid=※ name=mesh vtx=#")
        self.assertLessEqual(len(signature_of("x" * 500)), 96)

    def test_id_diff_equal(self):
        up = "node CAB-" + "a" * 32 + "\nclass=43 pid=123\nclass=43 pid=-9\n"
        d = id_diff(up, up)
        self.assertTrue(d["cabEqual"])
        self.assertEqual(d["pidJaccard"], 1.0)
        self.assertEqual(d["pidUpOnly"], 0)
        self.assertEqual(d["pidOursOnly"], 0)

    def test_id_diff_divergent(self):
        up = "CAB-" + "a" * 32 + "\npid=1\npid=2\n"
        ours = "CAB-" + "b" * 32 + "\npid=1\npid=3\n"
        d = id_diff(up, ours)
        self.assertFalse(d["cabEqual"])
        self.assertEqual(d["pidJaccard"], round(1 / 3, 6))
        self.assertEqual(d["pidUpOnly"], 1)
        self.assertEqual(d["pidOursOnly"], 1)
        self.assertTrue(any(s["pid"] == 2 for s in d["samples"]))

    def test_id_diff_multiset(self):
        d = id_diff("pid=7\npid=7\n", "pid=7\n")
        self.assertEqual(d["pidJaccard"], 0.5)
        self.assertEqual(d["pidUpOnly"], 1)

    def test_id_diff_empty(self):
        d = id_diff("", "")
        self.assertEqual(d["pidJaccard"], 1.0)
        self.assertTrue(d["cabEqual"])

    def test_struct_from_texts_identical_and_pinned(self):
        up = "== hdr\nclass=43 pid=11 name=mesh\nclass=142 deps=CAB-" + "a" * 32
        ours = "== hdr\nclass=43 pid=99 name=mesh\nclass=142 deps=CAB-" + "b" * 32
        d = struct_diff_from_texts(up, ours)
        self.assertEqual(d["class"], "structIdentical")
        self.assertEqual(d["pinnedDiffLines"], 0)
        ours2 = "class=43 pid=99 name=mesh\nclass=142 ts=1700000000"
        d2 = struct_diff_from_texts(up, ours2)
        self.assertEqual(d2["class"], "structIdentical")
        self.assertEqual(d2["pinnedDiffLines"], 2)
        self.assertEqual(len(d2["pinnedSamples"]), 2)

    def test_struct_from_texts_diff(self):
        d = struct_diff_from_texts("class=43 pid=1 name=a",
                                   "class=43 pid=1 name=b")
        self.assertEqual(d["class"], "structDiff")
        self.assertEqual(d["upOnly"], 1)
        self.assertEqual(d["oursOnly"], 1)
        self.assertEqual(len(d["samples"]), 2)

    def test_vol_norm(self):
        self.assertEqual(vol_norm("pid=42 CAB-" + "0" * 32),
                         "pid=※ CAB-…")


class TestVerdictMetrics(unittest.TestCase):
    def test_bundle_set_norm(self):
        man = {"files": ["abc_windows", "def_" + "0" * 32 + "_windows",
                         "dcl", "x.json", "entity_y", "scene_ignore_windows"]}
        self.assertEqual(verdict.bundle_set(man, "windows"), {"abc", "def"})
        self.assertEqual(norm_bundle_name("QmXYZ_windows", "windows"),
                         ("bundle", "qmxyz"))

    def _m1_entities(self, ofiles, ufiles):
        return [{"entity": "e1", "platform": "windows",
                 "ours": {"files": ofiles, "exitCode": 0},
                 "upstream": {"files": ufiles, "exitCode": 0}}]

    def test_m1_pass_and_fail(self):
        g = {"min_set_equal_rate": 1.0, "ours_exit_codes": [0],
             "upstream_exit_codes": [0, 12], "max_unpaired": 0}
        ents = self._m1_entities(["a_windows"], ["a_windows"])
        r = verdict.eval_m1(ents, [], g, set())
        self.assertEqual(r["status"], "PASS")
        ents = self._m1_entities(["a_windows", "b_windows"], ["a_windows"])
        r = verdict.eval_m1(ents, [{"status": "unpairable"}], g, set())
        self.assertEqual(r["status"], "FAIL")
        self.assertEqual(r["offenders"][0]["onlyOurs"], ["b"])
        r = verdict.eval_m1(ents, [{"status": "unpairable"}], g, {"m1:e1"})
        self.assertEqual(r["status"], "FAIL")
        g2 = dict(g, min_set_equal_rate=0.0, max_unpaired=None)
        r = verdict.eval_m1(ents, [], g2, {"m1:e1"})
        self.assertEqual(r["status"], "PASS")

    def test_m1_exit_codes(self):
        g = {"min_set_equal_rate": 0.0, "ours_exit_codes": [0],
             "upstream_exit_codes": [0], "max_unpaired": None}
        ents = [{"entity": "e1", "platform": "windows",
                 "ours": {"files": ["a_windows"], "exitCode": 0},
                 "upstream": {"files": ["a_windows"], "exitCode": 12}}]
        self.assertEqual(verdict.eval_m1(ents, [], g, set())["status"], "FAIL")

    def test_m2(self):
        g = {"max_load_fail": 0}
        ok = [{"pair": "w1", "class": "structIdentical"}]
        self.assertEqual(verdict.eval_m2(ok, g)["status"], "PASS")
        bad = ok + [{"pair": "w2", "class": "loadFailOurs"}]
        self.assertEqual(verdict.eval_m2(bad, g)["status"], "FAIL")
        self.assertEqual(verdict.eval_m2([], g)["status"], "NOT-RUN")
        missing = [{"pair": "w1", "error": "objdump tool missing"}]
        self.assertEqual(verdict.eval_m2(missing, g)["status"], "NOT-RUN")

    def test_m3(self):
        rows = [{"pair": "w1", "bundle": "b1", "cabEqual": True,
                 "pidJaccard": 1.0},
                {"pair": "w2", "bundle": "b2", "cabEqual": True,
                 "pidJaccard": 0.9}]
        info = verdict.eval_m3(rows, {"gated": False}, set())
        self.assertEqual(info["status"], "INFO")
        g = {"gated": True, "min_cab_equal_rate": 1.0, "min_pid_ok_rate": 0.9}
        self.assertEqual(verdict.eval_m3(rows, g, set())["status"], "FAIL")
        r = verdict.eval_m3(rows, g, {"m3:b2"})
        self.assertEqual(r["status"], "FAIL")
        g2 = dict(g, min_pid_ok_rate=0.5)
        self.assertEqual(verdict.eval_m3(rows, g2, {"m3:b2"})["status"], "PASS")
        self.assertEqual(verdict.eval_m3([], g, set())["status"], "NOT-RUN")

    def test_m4(self):
        g = {"max_fail_rate": 0.0, "min_ident_rate": 0.5,
             "allowlist_required": True}
        struct = [
            {"pair": "w1", "class": "structIdentical"},
            {"pair": "w2", "class": "structDiff",
             "samples": [{"up": "class=43 name=oddball vtx=7", "ours": None}]},
        ]
        bytes_ = [{"pair": "w1", "identical": False},
                  {"pair": "w2", "identical": False}]
        r = verdict.eval_m4(struct, bytes_, g, set())
        self.assertEqual(r["status"], "FAIL")
        sig = r["newSignatures"][0]["signature"]
        self.assertEqual(sig, signature_of("class=43 name=oddball vtx=7"))
        r = verdict.eval_m4(struct, bytes_, g, {sig})
        self.assertEqual(r["status"], "PASS")
        withfail = struct + [{"pair": "w3", "class": "loadFailOurs"}]
        r = verdict.eval_m4(withfail, bytes_, g, {sig})
        self.assertEqual(r["status"], "FAIL")

    def test_m4_byte_identical_counts(self):
        g = {"max_fail_rate": 0.0, "min_ident_rate": 1.0,
             "allowlist_required": True}
        struct = [{"pair": "w1", "class": "structDiff", "samples": []}]
        bytes_ = [{"pair": "w1", "identical": True}]
        self.assertEqual(verdict.eval_m4(struct, bytes_, g, set())["status"],
                         "PASS")

    def test_m5(self):
        g = {"min_ok_rate": 1.0, "ok_classes": ["identical", "identical-decode"],
             "max_fail": 0, "max_stub": 0}
        rows = [{"pair": "w1", "bundle": "t1", "class": "identical-decode",
                 "label": "identical"}]
        self.assertEqual(verdict.eval_m5(rows, g, set())["status"], "PASS")
        rows.append({"pair": "w2", "bundle": "t2", "class": "visible",
                     "label": "visible"})
        self.assertEqual(verdict.eval_m5(rows, g, set())["status"], "FAIL")
        gp = {"min_ok_rate": 0.5, "ok_classes":
              ["identical", "identical-decode", "imperceptible"],
              "max_ppm": 200, "max_fail": 0, "max_stub": 0}
        r = verdict.eval_m5(rows, gp, {"m5:t2"})
        self.assertEqual(r["status"], "PASS")
        self.assertEqual(verdict.eval_m5([], g, set())["status"], "NOT-RUN")

    def test_m5_ppm_bound(self):
        gp = {"min_ok_rate": 1.0, "ok_classes":
              ["identical", "identical-decode", "imperceptible"],
              "max_ppm": 200, "max_fail": 0, "max_stub": 0}
        rows = [{"pair": "w1", "bundle": "t1", "class": "imperceptible",
                 "label": "imperceptible", "ppm": 300}]
        self.assertEqual(verdict.eval_m5(rows, gp, {"m5:t1"})["status"], "FAIL")

    def test_m6(self):
        bundle_of = {"w1": "b1"}
        rows = [{"pair": "w1", "class": "structIdentical",
                 "pinnedDiffLines": 2,
                 "pinnedSamples": [{"up": "class=142 ts=170", "ours": None}]}]
        local = {"max_pinned_lines": 0}
        self.assertEqual(verdict.eval_m6(rows, local, set(), bundle_of)["status"],
                         "FAIL")
        self.assertEqual(verdict.eval_m6(rows, local, {"m6:b1"},
                                         bundle_of)["status"], "PASS")
        prod = {"max_pinned_lines": None,
                "expected_pin_patterns": ["^class=142 "]}
        self.assertEqual(verdict.eval_m6(rows, prod, set(), bundle_of)["status"],
                         "PASS")
        prod2 = {"max_pinned_lines": None,
                 "expected_pin_patterns": ["^class=999 "]}
        self.assertEqual(verdict.eval_m6(rows, prod2, set(),
                                         bundle_of)["status"], "FAIL")

    def test_m8(self):
        g = {"require_all_detected": True, "max_skipped": 0}
        self.assertEqual(verdict.eval_m8(None, g)["status"], "NOT-RUN")
        ok = {"all_detected": True, "skipped": [], "blindspots": []}
        self.assertEqual(verdict.eval_m8(ok, g)["status"], "PASS")
        sk = {"all_detected": True, "skipped": ["NC4"], "blindspots": []}
        self.assertEqual(verdict.eval_m8(sk, g)["status"], "FAIL")
        nd = {"all_detected": False, "skipped": [], "blindspots": []}
        self.assertEqual(verdict.eval_m8(nd, g)["status"], "FAIL")

    def test_allow_set_requires_filled_why(self):
        gates = {"allowlist": [
            {"signature": "sig-a", "why": "documented", "evidence": "x"},
            {"signature": "sig-b", "why": "", "evidence": "x"},
            {"signature": "sig-c"},
        ]}
        self.assertEqual(verdict.allow_set(gates), {"sig-a"})

    def test_cross_ours(self):
        tmp = tempfile.mkdtemp(prefix="crossours-test-")
        try:
            for run, content in (("ra", b"same"), ("rb", b"same")):
                d = os.path.join(tmp, run, "ours", "e", "windows")
                os.makedirs(d)
                with open(os.path.join(d, "f_windows"), "wb") as f:
                    f.write(content)
            r = verdict.cross_ours(os.path.join(tmp, "ra"),
                                   os.path.join(tmp, "rb"))
            self.assertEqual(r["status"], "PASS")
            with open(os.path.join(tmp, "rb", "ours", "e", "windows",
                                   "f_windows"), "wb") as f:
                f.write(b"drift")
            r = verdict.cross_ours(os.path.join(tmp, "ra"),
                                   os.path.join(tmp, "rb"))
            self.assertEqual(r["status"], "FAIL")
            self.assertEqual(r["value"]["shaMismatch"], 1)
        finally:
            shutil.rmtree(tmp)


if __name__ == "__main__":
    unittest.main()
