"""Local abgen JIT server: reuse a healthy one or spawn a scratch instance.

Spawned instances get a scratch out_root INSIDE the run dir (ours-out/) plus a
scratch cache (ours-cache/), so the run keeps full byte provenance. The binary
is the repo's target/release/abgen (result/bin/abgen fallback). ABGEN_ROOT is
pointed at the repo root so the vendored template/ bundles are found.
"""
import glob
import json
import os
import subprocess
import time

from .util import free_port, http_get

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


def _binary():
    for rel in ("target/release/abgen", "result/bin/abgen"):
        p = os.path.join(REPO_ROOT, rel)
        if os.path.isfile(p) and os.access(p, os.X_OK):
            return p
    raise SystemExit(
        "abgen server binary not found — build it first: "
        "cargo build --release (or nix build .#) in " + REPO_ROOT
    )


def _turbojpeg_lib():
    if os.environ.get("TURBOJPEG_LIB"):
        return os.environ["TURBOJPEG_LIB"]
    for pat in (
        "/nix/store/*libjpeg*turbo*/lib/libturbojpeg.so",
        "/usr/lib/*/libturbojpeg.so*",
        "/usr/lib/libturbojpeg.so*",
        "/nix/store/*libjpeg*turbo*/lib/libturbojpeg.dylib",
        "/opt/homebrew/lib/libturbojpeg.dylib",
        "/usr/local/lib/libturbojpeg.dylib",
    ):
        hits = sorted(glob.glob(pat))
        if hits:
            return hits[0]
    return None


def server_health(url):
    status, body = http_get(url.rstrip("/") + "/health", timeout=4, retries=0)
    if status is None or not body:
        return None
    try:
        return status, json.loads(body)
    except ValueError:
        return None


class LocalServer:
    """Context manager: .url usable after __enter__; spawned process reaped."""

    def __init__(self, run_dir, content_url, prefer_url=None, log=print):
        self.run_dir = run_dir
        self.content_url = content_url
        self.prefer_url = prefer_url or "http://127.0.0.1:5147"
        self.log = log
        self.proc = None
        self.url = None
        self.spawned = False

    def __enter__(self):
        h = server_health(self.prefer_url)
        if h and h[0] == 200:
            self.url = self.prefer_url
            self.log(f"ours-server: reusing healthy abgen server at {self.url}")
            return self
        self.spawn()
        return self

    def spawn(self):
        port = free_port()
        env = dict(os.environ)
        env.update(
            {
                "HTTP_SERVER_HOST": "127.0.0.1",
                "HTTP_SERVER_PORT": str(port),
                "ABGEN_OUT_ROOT": os.path.join(self.run_dir, "ours-out"),
                "ABGEN_CACHE_DIR": os.path.join(self.run_dir, "ours-cache"),
                "ABGEN_CATALYST_URL": self.content_url,
                "ABGEN_MANIFEST_CONTENT_SERVER_URL": self.content_url,
                "ABGEN_ROOT": REPO_ROOT,
            }
        )
        tj = _turbojpeg_lib()
        if tj:
            env["TURBOJPEG_LIB"] = tj
        else:
            self.log("ours-server: WARN no libturbojpeg found — the server "
                     "still dlopens platform sonames itself, else falls back "
                     "to vendored libjpeg9c (valid output, NOT byte-parity "
                     "with production for jpg textures)")
        os.makedirs(env["ABGEN_OUT_ROOT"], exist_ok=True)
        os.makedirs(env["ABGEN_CACHE_DIR"], exist_ok=True)
        logf = open(os.path.join(self.run_dir, "ours-server.log"), "ab")
        self.proc = subprocess.Popen(
            [_binary()], env=env, stdout=logf, stderr=subprocess.STDOUT
        )
        self.url = f"http://127.0.0.1:{port}"
        self.spawned = True
        deadline = time.time() + 30
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise SystemExit(
                    f"abgen server exited immediately (rc={self.proc.returncode}) — "
                    f"see {self.run_dir}/ours-server.log"
                )
            h = server_health(self.url)
            if h:
                st, body = h
                self.log(
                    f"ours-server: spawned pid={self.proc.pid} {self.url} "
                    f"health={st} mode={body.get('mode')} template_ok={body.get('template_ok')}"
                )
                if st != 200:
                    self.log(f"ours-server: WARN degraded health: {body}")
                return
            time.sleep(0.4)
        raise SystemExit("abgen server did not become healthy within 30s")

    def __exit__(self, *exc):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
            self.log(f"ours-server: stopped pid={self.proc.pid}")
        return False
