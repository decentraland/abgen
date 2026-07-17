"""Mechanical simval gate evaluator: scores a run's archived analysis files
against pipeline/simval-gates.json for the run's tier. Every metric reports
PASS / FAIL / NOT-RUN / INFO explicitly — the current pipeline degrades
silently (missing tool = missing rows); this layer exists so a NOT-RUN can
never masquerade as a pass.

In-run: evaluate_run(run_dir, tier) — the CLI writes analysis/verdict.json
before the COMPLETE marker. Standalone (re-gate after allowlist triage
without redoing runs; runs are immutable once COMPLETE):

  python3 -m abgencompare.verdict <run-dir> [--tier prod|local]
      [--expect-fail] [--cross-ours <other-run>]

writes a SIBLING runs/<id>.gate.json. --cross-ours implements the M0a sha256
ours-tree comparison; --expect-fail inverts the exit code (0 iff at least one
gated metric FAILS — for negative-control runs).

Allowlist entries ({signature, why, evidence}) only count when `why` is
filled — compat-bag discipline: a pass can never silently absorb a new
divergence class. M4 signature coverage is over the bounded samples recorded
in structure.jsonl (12 per pair).
"""
import argparse
import hashlib
import json
import os
import re

from .analyze import signature_of
from .fetch import norm_bundle_name
from .util import read_jsonl, write_json

GATES_PATH = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "simval-gates.json")


def load_gates(path=None):
    with open(path or GATES_PATH) as f:
        return json.load(f)


def allow_set(gates):
    return {e["signature"] for e in gates.get("allowlist", [])
            if e.get("signature") and e.get("why")}


def bundle_set(man, platform):
    out = set()
    for f in (man or {}).get("files", []):
        kind, cid = norm_bundle_name(f, platform)
        if kind == "bundle":
            out.add(cid)
    return out


def _rate(n, d):
    return round(n / d, 4) if d else None


def eval_m1(entities, pair_rows, g, allow):
    if not entities:
        return {"status": "NOT-RUN", "reason": "no archived manifests"}
    equal, offenders, exit_bad = 0, [], []
    for e in entities:
        oset = bundle_set(e["ours"], e["platform"])
        uset = bundle_set(e["upstream"], e["platform"])
        if oset == uset and oset:
            equal += 1
        else:
            offenders.append({
                "entity": e["entity"],
                "onlyOurs": sorted(oset - uset)[:8],
                "onlyUpstream": sorted(uset - oset)[:8],
                "allowlisted": f"m1:{e['entity']}" in allow,
            })
        oe = (e["ours"] or {}).get("exitCode")
        ue = (e["upstream"] or {}).get("exitCode")
        if oe not in g["ours_exit_codes"] or ue not in g["upstream_exit_codes"]:
            exit_bad.append({"entity": e["entity"], "ours": oe, "upstream": ue})
    unpaired = sum(1 for r in pair_rows
                   if r["status"] in ("unpairable", "ours-unfetchable"))
    unexplained = [o["entity"] for o in offenders if not o["allowlisted"]]
    rate = _rate(equal, len(entities))
    ok = (rate is not None and rate >= g["min_set_equal_rate"]
          and not unexplained and not exit_bad)
    if g.get("max_unpaired") is not None:
        ok = ok and unpaired <= g["max_unpaired"]
    return {"status": "PASS" if ok else "FAIL",
            "value": {"setEqualRate": rate, "entities": len(entities),
                      "unpaired": unpaired},
            "threshold": g, "offenders": offenders, "exitBad": exit_bad,
            "unexplained": unexplained}


def eval_m2(struct_rows, g):
    if not struct_rows:
        return {"status": "NOT-RUN", "reason": "no structure rows"}
    missing = [r["pair"] for r in struct_rows
               if "class" not in r and r.get("error")]
    if missing:
        return {"status": "NOT-RUN",
                "reason": f"objdump unavailable for {len(missing)} pair(s)",
                "offenders": missing[:12]}
    bad = [r["pair"] for r in struct_rows
           if r.get("class") in ("loadFailOurs", "loadFailUpstream")]
    return {"status": "PASS" if len(bad) <= g["max_load_fail"] else "FAIL",
            "value": {"loadFail": len(bad), "pairs": len(struct_rows)},
            "threshold": g, "offenders": bad[:12]}


def eval_m3(id_rows, g, allow):
    if not id_rows:
        return {"status": "NOT-RUN", "reason": "no iddiff rows"}
    n = len(id_rows)
    cab_ok = sum(1 for r in id_rows if r["cabEqual"])
    pid_ok = sum(1 for r in id_rows if r["pidJaccard"] == 1.0)
    offenders = [{"pair": r["pair"], "bundle": r.get("bundle"),
                  "cabEqual": r["cabEqual"], "pidJaccard": r["pidJaccard"],
                  "allowlisted": f"m3:{r.get('bundle')}" in allow}
                 for r in id_rows if not r["cabEqual"] or r["pidJaccard"] < 1.0]
    value = {"pairs": n, "cabEqualRate": _rate(cab_ok, n),
             "pidExactRate": _rate(pid_ok, n)}
    if not g.get("gated"):
        return {"status": "INFO", "value": value, "offenders": offenders[:12]}
    unexplained = [o["pair"] for o in offenders if not o["allowlisted"]]
    ok = (value["cabEqualRate"] >= g["min_cab_equal_rate"]
          and value["pidExactRate"] >= g["min_pid_ok_rate"]
          and not unexplained)
    return {"status": "PASS" if ok else "FAIL", "value": value,
            "threshold": g, "offenders": offenders[:12],
            "unexplained": unexplained}


def eval_m4(struct_rows, byte_rows, g, allow):
    rows = [r for r in struct_rows if r.get("class")]
    if not rows:
        return {"status": "NOT-RUN", "reason": "no structure rows"}
    ident_bytes = {r["pair"] for r in byte_rows if r.get("identical")}
    n = len(rows)
    fails = [r["pair"] for r in rows
             if r["class"] not in ("structIdentical", "structDiff")]
    ident = sum(1 for r in rows
                if r["class"] == "structIdentical" or r["pair"] in ident_bytes)
    new_sigs = {}
    for r in rows:
        if r["class"] != "structDiff":
            continue
        for s in r.get("samples", []):
            sig = signature_of(s.get("up") or s.get("ours") or "")
            if sig and sig not in allow:
                new_sigs.setdefault(sig, s.get("up") or s.get("ours"))
    value = {"pairs": n, "failRate": _rate(len(fails), n),
             "identRate": _rate(ident, n), "newSignatures": len(new_sigs)}
    ok = (value["failRate"] <= g["max_fail_rate"]
          and value["identRate"] >= g["min_ident_rate"]
          and (not g.get("allowlist_required") or not new_sigs))
    return {"status": "PASS" if ok else "FAIL", "value": value,
            "threshold": g, "offenders": fails[:12],
            "newSignatures": [{"signature": k, "sample": v}
                              for k, v in sorted(new_sigs.items())[:20]]}


def eval_m5(tex_rows, g, allow):
    if not tex_rows:
        return {"status": "NOT-RUN", "reason": "no texture pairs"}
    scored = [r for r in tex_rows if r.get("class")]
    if not scored:
        return {"status": "NOT-RUN", "reason": "texcmp/texpng tools missing"}
    n = len(scored)
    stubs = [r["pair"] for r in scored if r.get("corpusStub")]
    fails = [r["pair"] for r in scored if r.get("label") == "fail"]

    def ok_row(r):
        cls = r["class"]
        if cls in ("identical", "identical-decode"):
            return cls in g["ok_classes"]
        if cls == "imperceptible" and "imperceptible" in g["ok_classes"]:
            ppm = r.get("ppm")
            return ppm is None or ppm <= g.get("max_ppm", 200)
        return False

    ok_n = sum(1 for r in scored if ok_row(r))
    visible = [{"pair": r["pair"], "bundle": r.get("bundle"),
                "allowlisted": f"m5:{r.get('bundle')}" in allow}
               for r in scored if not ok_row(r) and r.get("label") != "fail"]
    unexplained = [v["pair"] for v in visible if not v["allowlisted"]]
    value = {"pairs": n, "okRate": _rate(ok_n, n), "fail": len(fails),
             "stub": len(stubs)}
    ok = (value["okRate"] >= g["min_ok_rate"] and len(fails) <= g["max_fail"]
          and len(stubs) <= g["max_stub"] and not unexplained)
    return {"status": "PASS" if ok else "FAIL", "value": value,
            "threshold": g, "offenders": (fails + stubs)[:12],
            "visible": visible[:12], "unexplained": unexplained}


def eval_m6(struct_rows, g, allow, bundle_of):
    rows = [r for r in struct_rows if r.get("class") in
            ("structIdentical", "structDiff")]
    if not rows:
        return {"status": "NOT-RUN", "reason": "no structure rows"}
    pinned = [r for r in rows if r.get("pinnedDiffLines")]
    value = {"pairs": len(rows), "pairsWithPins": len(pinned),
             "pinnedLines": sum(r["pinnedDiffLines"] for r in pinned)}
    if g.get("max_pinned_lines") is not None:
        offenders = [{"pair": r["pair"], "bundle": bundle_of.get(r["pair"]),
                      "pinnedDiffLines": r["pinnedDiffLines"],
                      "allowlisted": f"m6:{bundle_of.get(r['pair'])}" in allow}
                     for r in pinned
                     if r["pinnedDiffLines"] > g["max_pinned_lines"]]
        unexplained = [o["pair"] for o in offenders if not o["allowlisted"]]
        return {"status": "PASS" if not unexplained else "FAIL",
                "value": value, "threshold": g, "offenders": offenders[:12],
                "unexplained": unexplained}
    pats = [re.compile(p) for p in g.get("expected_pin_patterns", [])]
    if not pats:
        return {"status": "INFO", "value": value}
    bad = []
    for r in pinned:
        for s in r.get("pinnedSamples", []):
            line = s.get("up") or s.get("ours") or ""
            if not any(p.search(line) for p in pats):
                bad.append({"pair": r["pair"], "line": line[:120]})
    return {"status": "PASS" if not bad else "FAIL", "value": value,
            "threshold": g, "offenders": bad[:12]}


def eval_m8(selftest, g):
    if selftest is None:
        return {"status": "NOT-RUN", "reason": "selftest not run (--selftest)"}
    skipped = selftest.get("skipped", [])
    ok = (selftest.get("all_detected") is True
          and len(skipped) <= g.get("max_skipped", 0))
    return {"status": "PASS" if ok else "FAIL",
            "value": {"all_detected": selftest.get("all_detected"),
                      "skipped": skipped,
                      "blindspots": selftest.get("blindspots", [])},
            "threshold": g}


def _tree_sha(root):
    out = {}
    for dirpath, _dirs, files in os.walk(root):
        for f in files:
            p = os.path.join(dirpath, f)
            with open(p, "rb") as fh:
                out[os.path.relpath(p, root)] = hashlib.sha256(
                    fh.read()).hexdigest()
    return out


def cross_ours(run_a, run_b):
    """M0a: sha256-compare the archived ours/ trees of two runs."""
    ta = _tree_sha(os.path.join(run_a, "ours"))
    tb = _tree_sha(os.path.join(run_b, "ours"))
    if not ta or not tb:
        return {"status": "NOT-RUN", "reason": "one or both ours/ trees empty"}
    only_a = sorted(set(ta) - set(tb))
    only_b = sorted(set(tb) - set(ta))
    mismatch = sorted(k for k in set(ta) & set(tb) if ta[k] != tb[k])
    ok = not only_a and not only_b and not mismatch
    return {"status": "PASS" if ok else "FAIL",
            "value": {"filesA": len(ta), "filesB": len(tb),
                      "onlyA": len(only_a), "onlyB": len(only_b),
                      "shaMismatch": len(mismatch)},
            "offenders": (only_a + only_b + mismatch)[:12],
            "runs": [os.path.abspath(run_a), os.path.abspath(run_b)]}


def _load_entities(run_dir, platform):
    mdir = os.path.join(run_dir, "manifests")
    names = set()
    if os.path.isdir(mdir):
        for fn in os.listdir(mdir):
            for suf in (".ours.json", ".upstream.json"):
                if fn.endswith(suf):
                    names.add(fn[:-len(suf)])
    out = []
    for e in sorted(names):
        sides = {}
        for side in ("ours", "upstream"):
            try:
                with open(os.path.join(mdir, f"{e}.{side}.json")) as f:
                    sides[side] = json.load(f)
            except (OSError, ValueError):
                sides[side] = None
        out.append({"entity": e, "platform": platform, **sides})
    return out


def evaluate_run(run_dir, tier=None, gates_path=None):
    run_dir = os.path.abspath(run_dir)
    with open(os.path.join(run_dir, "config.json")) as f:
        config = json.load(f)
    platform = config["platform"]
    tier = tier or config.get("tier") or "prod"
    gates = load_gates(gates_path)
    tg = gates["tiers"][tier]
    allow = allow_set(gates)
    adir = os.path.join(run_dir, "analysis")
    pair_rows = read_jsonl(os.path.join(adir, "pairs.jsonl"))
    byte_rows = read_jsonl(os.path.join(adir, "bytediff.jsonl"))
    struct_rows = read_jsonl(os.path.join(adir, "structure.jsonl"))
    id_rows = read_jsonl(os.path.join(adir, "iddiff.jsonl"))
    matrix = read_jsonl(os.path.join(adir, "matrix.jsonl"))
    tex_rows = [r for r in matrix
                if r.get("rev") == 1 and r.get("kind") == "texture"]
    selftest = None
    try:
        with open(os.path.join(adir, "selftest.json")) as f:
            selftest = json.load(f)
    except (OSError, ValueError):
        pass
    bundle_of = {r["pair_id"]: r["cid"] for r in pair_rows if r.get("pair_id")}
    metrics = {
        "M1-fileset": eval_m1(_load_entities(run_dir, platform), pair_rows,
                              tg["m1"], allow),
        "M2-container": eval_m2(struct_rows, tg["m2"]),
        "M3-identity": eval_m3(id_rows, tg["m3"], allow),
        "M4-structure": eval_m4(struct_rows, byte_rows, tg["m4"], allow),
        "M5-textures": eval_m5(tex_rows, tg["m5"], allow),
        "M6-metadata-pins": eval_m6(struct_rows, tg["m6"], allow, bundle_of),
        "M8-falsifiability": eval_m8(selftest, tg["m8"]),
    }
    return {
        "run": config.get("run_id") or os.path.basename(run_dir),
        "run_dir": run_dir,
        "tier": tier,
        "platform": platform,
        "metrics": metrics,
        "pass": not any(m["status"] == "FAIL" for m in metrics.values()),
        "not_run": sorted(k for k, m in metrics.items()
                          if m["status"] == "NOT-RUN"),
    }


def main(argv=None):
    ap = argparse.ArgumentParser(prog="python3 -m abgencompare.verdict",
                                 description=__doc__)
    ap.add_argument("run_dir")
    ap.add_argument("--tier", choices=("prod", "local"))
    ap.add_argument("--gates")
    ap.add_argument("--expect-fail", action="store_true")
    ap.add_argument("--cross-ours", metavar="OTHER_RUN")
    args = ap.parse_args(argv)
    v = evaluate_run(args.run_dir, tier=args.tier, gates_path=args.gates)
    if args.cross_ours:
        v["metrics"]["M0a-determinism"] = cross_ours(args.run_dir,
                                                     args.cross_ours)
        v["pass"] = not any(m["status"] == "FAIL"
                            for m in v["metrics"].values())
        v["not_run"] = sorted(k for k, m in v["metrics"].items()
                              if m["status"] == "NOT-RUN")
    run_dir = os.path.abspath(args.run_dir).rstrip("/")
    out = run_dir + ".gate.json"
    write_json(out, v, indent=1)
    print(json.dumps({
        "gate": out, "tier": v["tier"], "pass": v["pass"],
        "not_run": v["not_run"],
        "metrics": {k: m["status"] for k, m in sorted(v["metrics"].items())},
    }, indent=1))
    failed = any(m["status"] == "FAIL" for m in v["metrics"].values())
    if args.expect_fail:
        return 0 if failed else 1
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
