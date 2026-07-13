"""Shared helpers: HTTP (urllib, stdlib only), jsonl IO, run logging."""
import json
import os
import socket
import sys
import time
import urllib.error
import urllib.request

UA = {"User-Agent": "abgen-compare/0.1 (read-only parity pipeline)"}


def http_get(url, timeout=60, retries=2, sleep=0.6):
    """GET url -> (status, bytes|None). Network errors return (None, errstr-bytes)."""
    last = (None, b"no attempt")
    for attempt in range(retries + 1):
        req = urllib.request.Request(url, headers=UA)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return r.status, r.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read() if e.fp else None
        except Exception as e:  # noqa: BLE001 — URLError, timeout, reset
            last = (None, str(e).encode())
            time.sleep(sleep * (attempt + 1))
    return last


def http_post_json(url, body, timeout=60):
    data = json.dumps(body).encode()
    req = urllib.request.Request(
        url, data=data, headers={**UA, "Content-Type": "application/json"}
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read() if e.fp else None
    except Exception as e:  # noqa: BLE001
        return None, str(e).encode()


def read_jsonl(path):
    out = []
    try:
        with open(path) as f:
            for line in f:
                line = line.strip()
                if line:
                    out.append(json.loads(line))
    except FileNotFoundError:
        pass
    return out


def append_jsonl(path, rows):
    """Atomic-ish append: one write() call for the whole blob."""
    blob = "".join(json.dumps(r) + "\n" for r in rows)
    with open(path, "a") as f:
        f.write(blob)


def write_json(path, obj, indent=None):
    tmp = f"{path}.tmp.{os.getpid()}"
    with open(tmp, "w") as f:
        json.dump(obj, f, indent=indent)
    os.replace(tmp, path)


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


class RunLog:
    """Stage journal: appends timestamped lines to <run>/run.log and stderr."""

    def __init__(self, run_dir):
        self.path = os.path.join(run_dir, "run.log")

    def __call__(self, msg):
        line = f"{time.strftime('%Y-%m-%dT%H:%M:%S')} {msg}"
        print(line, file=sys.stderr)
        with open(self.path, "a") as f:
            f.write(line + "\n")
