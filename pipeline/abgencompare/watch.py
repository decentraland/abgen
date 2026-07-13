"""Live JIT compare: watch an abgen JIT server's generated-output tree and
maintain ONE rolling run comparing every converted entity against upstream.

Watched layout (the abgen server's ABGEN_OUT_ROOT):

    <generated>/<entityId>/<platform>.manifest.json     completion marker
    <generated>/<entityId>/<platform>/<bundleFile>      payload bytes

The rolling run lives at <runs>/<run-id>/ in the standard run layout but is
NEVER marked COMPLETE: rows are replaced in place when the JIT re-converts an
entity. The dedupe key is entity+platform and pair ids are content-derived
(sha1 of entity|cid|platform), so a re-conversion overwrites exactly its own
rows. All run files are rewritten atomically (tmp+rename); the crash-safe
cursor lives in <run>/watch-state.json and is committed only AFTER an
entity's rows and site-data.json are flushed, so an interrupted entity is
simply reprocessed on restart. First start = backfill of every existing
generated entity, newest conversions first, one at a time (bounded, gentle on
the upstream CDN — per-payload --upstream-sleep applies).

Honesty rules (never fake a verdict):
- upstream manifest 404  -> entity SKIPPED with a visible `no_upstream`
  counter in site-data.json's `watch` block (upstream never converted it);
- transient failures (network, 5xx, resolve errors) -> retried with capped
  exponential backoff, surfaced in the `errors` counter, never rows;
- entities the content server no longer has -> `no_entity` counter.
"""
import hashlib
import json
import os
import shutil
import signal
import subprocess
import sys
import time
import traceback

from .fetch import fetch_side
from .headless import headless_matrix_rows
from .pair import build_pairs
from .resolve import entity_meta_row
from .runmodel import SUBDIRS, runs_root
from .sitedata import build_run_sitedata
from .util import http_get, read_jsonl, write_json

SITE_SERVER = os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
    "site", "server.py",
)

RETRY_BASE = 60.0
RETRY_MAX = 3600.0
META_MEMO_TTL = 3600.0


def now_iso():
    import datetime
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")


def log(msg):
    print(f"{time.strftime('%Y-%m-%dT%H:%M:%S')} {msg}", flush=True)


def live_pair_id(entity, cid, platform):
    """Deterministic pair id: re-processing an entity emits the SAME ids, so
    replacement (and deep links) are stable across re-conversions."""
    h = hashlib.sha1(f"{entity}|{cid}|{platform}".encode()).hexdigest()[:8]
    return f"{platform[0]}{h}"


def write_jsonl_atomic(path, rows):
    tmp = f"{path}.tmp.{os.getpid()}"
    with open(tmp, "w") as f:
        for r in rows:
            f.write(json.dumps(r) + "\n")
    os.replace(tmp, path)


class LiveRun:
    """The rolling run dir: in-memory row registries + atomic flush."""

    def __init__(self, root, run_id, cfg):
        self.run_id = run_id
        self.dir = os.path.join(root, run_id)
        os.makedirs(self.dir, exist_ok=True)
        if os.path.exists(os.path.join(self.dir, "COMPLETE")):
            raise SystemExit(
                f"{self.dir} is a COMPLETE (immutable) run — pick another --run-id")
        for d in SUBDIRS:
            os.makedirs(os.path.join(self.dir, d), exist_ok=True)
        self.config_path = os.path.join(self.dir, "config.json")
        prev = self._load_json(self.config_path) or {}
        self.config = {
            "run_id": run_id,
            "created": prev.get("created") or now_iso(),
            "mode": "live-jit-watch",
            "pointer": None,
            "platform": cfg["platforms_label"],
            "content_server": cfg["content"],
            "ab_cdn": cfg["abcdn"],
            "abgen_url": cfg["abgen_url"],
            "tags": ["live-jit"],
            "description": cfg["description"],
            "labels": {"upstream": "upstream ab-cdn", "ours": "abgen (live JIT)"},
            "thresholds": {"imperceptible_ppm": 200,
                           "amnesty": {"delta_gt": 8, "ppm": 200}},
            "rendered": False,
        }
        if self.config != prev:
            write_json(self.config_path, self.config, indent=1)
        self.state_path = os.path.join(self.dir, "watch-state.json")
        st = self._load_json(self.state_path) or {}
        self.keys = st.get("keys", {})
        self.pair_rows = {}
        for r in read_jsonl(os.path.join(self.dir, "analysis", "pairs.jsonl")):
            self.pair_rows[self._pair_key(r)] = r
        self.matrix = {}
        for r in read_jsonl(os.path.join(self.dir, "analysis", "matrix.jsonl")):
            self.matrix[r["pair"]] = r
        self.meta = {}
        for r in read_jsonl(os.path.join(self.dir, "entity-meta.jsonl")):
            self.meta[r["entity"]] = r

    @staticmethod
    def _load_json(path):
        try:
            with open(path) as f:
                return json.load(f)
        except (OSError, ValueError):
            return None

    @staticmethod
    def _pair_key(r):
        return r.get("pair_id") or f"unp|{r.get('entity')}|{r.get('platform')}|{r.get('cid')}"

    def replace(self, entity, platform, pair_rows, matrix_rows, meta_row):
        """Swap in the (entity, platform) slice: drop its old rows (and their
        decoded-texture images if the pair vanished), insert the new ones."""
        new_pids = {r["pair"] for r in matrix_rows}
        dropped = [pid for pid, r in self.matrix.items()
                   if r.get("entity") == entity and r.get("platform") == platform]
        for pid in dropped:
            del self.matrix[pid]
            if pid not in new_pids:
                for side in ("up", "ours"):
                    for suffix in (f"{pid}-{side}.png", f"{pid}-{side}.missing.txt"):
                        try:
                            os.remove(os.path.join(self.dir, "tex-images", suffix))
                        except OSError:
                            pass
        for k in [k for k, r in self.pair_rows.items()
                  if r.get("entity") == entity and r.get("platform") == platform]:
            del self.pair_rows[k]
        for r in pair_rows:
            self.pair_rows[self._pair_key(r)] = r
        for r in matrix_rows:
            self.matrix[r["pair"]] = r
        if meta_row:
            self.meta[entity] = meta_row

    def flush(self, watch_stats):
        write_jsonl_atomic(os.path.join(self.dir, "analysis", "pairs.jsonl"),
                           sorted(self.pair_rows.values(),
                                  key=lambda r: (r.get("entity") or "", r.get("cid") or "")))
        write_jsonl_atomic(os.path.join(self.dir, "analysis", "matrix.jsonl"),
                           sorted(self.matrix.values(), key=lambda r: r["pair"]))
        write_jsonl_atomic(os.path.join(self.dir, "entity-meta.jsonl"),
                           sorted(self.meta.values(), key=lambda r: r["entity"] or ""))
        sd = build_run_sitedata(self.dir, self.run_id, self.config)
        sd["watch"] = watch_stats
        write_json(os.path.join(self.dir, "site-data.json"), sd)

    def save_state(self):
        write_json(self.state_path, {"version": 1, "keys": self.keys}, indent=0)

    def drop_archives(self, entity, platform):
        for side in ("ours", "upstream"):
            shutil.rmtree(os.path.join(self.dir, side, entity, platform),
                          ignore_errors=True)


class Watcher:
    def __init__(self, args):
        self.args = args
        self.platforms = [p.strip() for p in args.platforms.split(",") if p.strip()]
        self.gen = os.path.abspath(args.jit_generated_dir)
        root = runs_root(args.runs_root)
        os.makedirs(root, exist_ok=True)
        self.run = LiveRun(root, args.run_id, {
            "platforms_label": ",".join(self.platforms),
            "content": args.content,
            "abcdn": args.abcdn,
            "abgen_url": args.abgen_url,
            "description": "live JIT compare — rolling headless parity of every "
                           "entity the abgen server converts, vs upstream ab-cdn",
        })
        self._meta_memo = {}
        self._pending = 0
        self._last_scan = None

    def scan(self):
        """-> {entity|platform: [mtime_ns, size]} of settled manifests."""
        found = {}
        now = time.time()
        try:
            names = os.listdir(self.gen)
        except OSError as e:
            log(f"scan: cannot list {self.gen}: {e}")
            return found
        for name in names:
            for plat in self.platforms:
                mpath = os.path.join(self.gen, name, f"{plat}.manifest.json")
                try:
                    st = os.stat(mpath)
                except OSError:
                    continue
                if now - st.st_mtime < self.args.settle:
                    continue
                found[f"{name}|{plat}"] = [st.st_mtime_ns, st.st_size]
        self._last_scan = now_iso()
        return found

    def plan(self, found):
        """-> (work [(key, sig)] newest-first, removed [key])."""
        now = time.time()
        work = []
        for key, sig in found.items():
            st = self.run.keys.get(key)
            if not st or st.get("sig") != sig:
                work.append((key, sig))
            elif st.get("status") == "error" and now >= st.get("next_retry", 0):
                work.append((key, sig))
        removed = []
        for key in list(self.run.keys):
            if key in found:
                continue
            entity, plat = key.split("|", 1)
            if not os.path.exists(
                    os.path.join(self.gen, entity, f"{plat}.manifest.json")):
                removed.append(key)
        work.sort(key=lambda kv: kv[1][0], reverse=True)
        return work, removed

    def stats(self):
        c = {}
        for v in self.run.keys.values():
            c[v.get("status", "error")] = c.get(v.get("status", "error"), 0) + 1
        purged = sum(1 for r in self.run.matrix.values()
                     if r.get("class") == "skipped")
        return {
            "live": True,
            "platforms": self.platforms,
            "keys_ok": c.get("ok", 0),
            "no_upstream": c.get("no-upstream", 0),
            "no_entity": c.get("no-entity", 0),
            "empty": c.get("empty", 0),
            "errors": c.get("error", 0),
            "purged_pairs": purged,
            "pending": self._pending,
            "last_scan": self._last_scan,
            "updated": now_iso(),
        }

    def _resolve(self, entity):
        """-> ('ok', meta_row) | ('no-entity', msg) | ('error', msg)."""
        memo = self._meta_memo.get(entity)
        if memo and time.time() - memo[0] < META_MEMO_TTL:
            return ("ok", memo[1])
        url = f"{self.args.content.rstrip('/')}/contents/{entity}"
        status, body = http_get(url, timeout=60)
        if status == 200 and body:
            try:
                ent = json.loads(body)
            except ValueError:
                return ("error", "entity JSON unparsable")
            ent.setdefault("id", entity)
            row = entity_meta_row(ent)
            if len(self._meta_memo) > 2048:
                self._meta_memo.clear()
            self._meta_memo[entity] = (time.time(), row)
            return ("ok", row)
        if status == 404:
            return ("no-entity", f"content server has no entity {entity}")
        return ("error", f"content server fetch status={status}")

    def _finish(self, key, entry, entity, plat, pair_rows, matrix_rows, meta_row):
        self.run.replace(entity, plat, pair_rows, matrix_rows, meta_row)
        self.run.keys[key] = entry
        self.run.flush(self.stats())
        self.run.save_state()

    def _error(self, key, sig, msg, prev):
        retries = 1
        if prev.get("status") == "error" and prev.get("sig") == sig:
            retries = prev.get("retries", 0) + 1
        backoff = min(RETRY_BASE * (2 ** (retries - 1)), RETRY_MAX)
        self.run.keys[key] = {
            "sig": sig, "status": "error", "error": msg[:300],
            "retries": retries, "next_retry": time.time() + backoff,
            "ts": now_iso(),
        }
        entity, plat = key.split("|", 1)
        log(f"{entity}|{plat}: ERROR {msg} (attempt {retries}, retry in {int(backoff)}s)")
        self.run.flush(self.stats())
        self.run.save_state()

    def process(self, key, sig, progress):
        entity, plat = key.split("|", 1)
        prev = self.run.keys.get(key, {})
        klog = lambda m: log(f"[{progress}] {entity}|{plat} {m}")  # noqa: E731
        kind, payload = self._resolve(entity)
        if kind == "no-entity":
            klog(f"skip: {payload}")
            self._finish(key, {"sig": sig, "status": "no-entity",
                               "note": payload, "ts": now_iso()},
                         entity, plat, [], [], None)
            return
        if kind == "error":
            self._error(key, sig, payload, prev)
            return
        meta_row = payload
        self.run.drop_archives(entity, plat)
        ours = fetch_side(self.args.abgen_url, "ours", entity, plat, self.run.dir,
                          klog, manifest_name=f"{entity}.{plat}.ours.json")
        if ours["manifest_status"] != 200:
            self._error(key, sig,
                        f"ours manifest status={ours['manifest_status']}", prev)
            return
        up = fetch_side(self.args.abcdn, "upstream", entity, plat, self.run.dir,
                        klog, sleep=self.args.upstream_sleep,
                        manifest_name=f"{entity}.{plat}.upstream.json")
        if up["manifest_status"] == 404:
            klog("skip: upstream never converted this entity (manifest 404) — "
                 "counted, no verdict")
            self.run.drop_archives(entity, plat)
            self._finish(key, {"sig": sig, "status": "no-upstream",
                               "ts": now_iso()}, entity, plat, [], [], meta_row)
            return
        if up["manifest_status"] != 200:
            self._error(key, sig,
                        f"upstream manifest status={up['manifest_status']}", prev)
            return
        pairs, _unfillable = build_pairs(self.run.run_id, plat, [meta_row],
                                         [ours], [up], klog)
        for r in pairs:
            if r.get("pair_id"):
                r["pair_id"] = live_pair_id(entity, r["cid"], plat)
        ts = now_iso()
        matrix_rows, _b, _s = headless_matrix_rows(self.run.dir, self.run.run_id,
                                                   plat, pairs, klog, ts)
        status = "ok" if ours["bundles"] else "empty"
        dist = {}
        for m in matrix_rows:
            dist[m["label"]] = dist.get(m["label"], 0) + 1
        klog(f"done: {len(matrix_rows)} row(s) {dist or ''} status={status}")
        self._finish(key, {"sig": sig, "status": status,
                           "rows": len(matrix_rows),
                           "pairs": sorted(r["pair"] for r in matrix_rows),
                           "ts": ts},
                     entity, plat, pairs, matrix_rows, meta_row)

    def remove(self, key):
        entity, plat = key.split("|", 1)
        log(f"{entity}|{plat}: generated dir vanished — dropping its rows")
        self.run.replace(entity, plat, [], [], None)
        self.run.keys.pop(key, None)
        self.run.drop_archives(entity, plat)


class ServeChild:
    """Supervised site server (site/server.py) beside the watch loop."""

    def __init__(self, port, host, runs):
        self.cmd = [sys.executable, SITE_SERVER, str(port),
                    "--runs", runs, "--host", host]
        self.proc = None

    def start(self):
        log("serve: " + " ".join(self.cmd))
        self.proc = subprocess.Popen(self.cmd)

    def ensure(self):
        if self.proc is not None and self.proc.poll() is not None:
            log(f"serve: site server exited rc={self.proc.returncode} — restarting")
            time.sleep(2)
            self.start()

    def stop(self):
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()


def _acquire_lock(run_dir):
    """One watcher per run dir (flock; kept for process lifetime)."""
    try:
        import fcntl
    except ImportError:
        return None
    fh = open(os.path.join(run_dir, "watch.lock"), "w")
    try:
        fcntl.flock(fh, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        raise SystemExit(
            f"another watcher already holds {run_dir}/watch.lock — "
            "run one watcher per rolling run")
    fh.write(f"{os.getpid()} {now_iso()}\n")
    fh.flush()
    return fh


def main(args):
    if not args.jit_generated_dir:
        raise SystemExit("watch: --jit-generated-dir (or ABGEN_JIT_GENERATED_DIR) "
                         "must point at the abgen server's out_root")
    if not os.path.isdir(args.jit_generated_dir):
        raise SystemExit(f"watch: {args.jit_generated_dir} is not a directory")
    w = Watcher(args)
    lock = _acquire_lock(w.run.dir)  # noqa: F841 — held for process lifetime
    server = None
    if args.serve:
        server = ServeChild(args.serve, args.host, runs_root(args.runs_root))
        server.start()

    def _sigterm(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, _sigterm)
    log(f"watch: generated={args.jit_generated_dir} run={w.run.dir} "
        f"platforms={w.platforms} interval={args.interval}s "
        f"abgen={args.abgen_url} abcdn={args.abcdn} content={args.content}")
    if not os.path.exists(os.path.join(w.run.dir, "site-data.json")):
        w.run.flush(w.stats())
    first = True
    try:
        while True:
            backlog = 0
            try:
                found = w.scan()
                work, removed = w.plan(found)
                if removed:
                    for key in removed:
                        w.remove(key)
                    w.run.flush(w.stats())
                    w.run.save_state()
                if work:
                    log(("backfill" if first else "scan") +
                        f": {len(work)} entity-platform conversion(s) to compare")
                    batch = work[:args.limit or args.batch]
                    backlog = len(work) - len(batch)
                    for i, (key, sig) in enumerate(batch):
                        w._pending = len(work) - i - 1
                        w.process(key, sig, f"{i + 1}/{len(work)}")
                        if server:
                            server.ensure()
                first = False
            except KeyboardInterrupt:
                raise
            except Exception:
                log("watch: cycle error (continuing):\n" + traceback.format_exc())
            if args.once:
                break
            if backlog:
                continue
            deadline = time.time() + args.interval
            while time.time() < deadline:
                if server:
                    server.ensure()
                time.sleep(1)
    except KeyboardInterrupt:
        log("watch: stopping")
    finally:
        if server:
            server.stop()
    return 0
