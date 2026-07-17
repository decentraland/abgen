"""Local-dir -> ab-cdn-URL-shape adapter (stdlib ThreadingHTTPServer,
loopback only). Lets the EXISTING --abcdn flag consume an on-disk snapshot:
fetch.py rejects file:// URLs (urllib FileHandler returns status=None), so a
tiny HTTP shim is the single missing piece.

Layouts:
  ref   ab-cdn-reference snapshot:
          <root>/<shard>/<entity>/<platform>.manifest.json
          <root>/<shard>/<entity>/<platform>/<file>
        shard = entity[:4] for Qm... ids (case-sensitive), entity[8:10] else.
  flat  det-guid convert-corpus.sh OUTPUT_DIR: <root>/<entity>/<hash>_<plat>;
        manifests are synthesized from the directory listing.

--drop <entity>/<file> (repeatable) omits the file from served/synthesized
manifests and 404s its payload — the drop-file negative control (NC5).

Usage: python3 -m abgencompare.dirserve --root DIR [--port 5148]
       [--layout ref|flat] [--drop ENTITY/FILE]...
"""
import argparse
import json
import os
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from . import PLATFORMS


def shard_of(entity):
    return entity[:4] if entity.startswith("Qm") else entity[8:10]


def split_manifest_name(name):
    """'<entity>_<platform>' | '<entity>' (webgl shape) -> (entity, platform)."""
    if "_" in name:
        entity, plat = name.rsplit("_", 1)
        if plat in PLATFORMS:
            return entity, plat
    return name, "webgl"


def platform_of_file(relf):
    tail = relf.rsplit("/", 1)[-1].rsplit("_", 1)[-1]
    return tail if tail in PLATFORMS else None


def manifest_response(root, layout, name, drops):
    """-> manifest body bytes | None (404). Verbatim on-disk bytes for the
    ref layout unless a --drop applies (byte-compare smokes rely on that)."""
    entity, plat = split_manifest_name(name)
    dropped = {f for e, f in drops if e == entity}
    if layout == "ref":
        path = os.path.join(root, shard_of(entity), entity,
                            plat + ".manifest.json")
        if not os.path.isfile(path):
            return None
        with open(path, "rb") as f:
            raw = f.read()
        if not dropped:
            return raw
        man = json.loads(raw)
        man["files"] = [x for x in man.get("files", []) if x not in dropped]
        return json.dumps(man, indent=1).encode()
    d = os.path.join(root, entity)
    if not os.path.isdir(d):
        return None
    files = sorted(n for n in os.listdir(d)
                   if n.endswith("_" + plat) and n not in dropped)
    return json.dumps({
        "version": "local-detguid",
        "files": files,
        "exitCode": 0,
        "date": "2020-01-01T00:00:00.000Z",
    }).encode()


def payload_path(root, layout, entity, relf, drops):
    """-> absolute on-disk path | None (404). The version URL segment is
    ignored: payloads are content-addressed by <entity>/<file>."""
    parts = relf.split("/")
    if ".." in parts or ".." in entity or (entity, relf) in drops:
        return None
    if layout == "ref":
        plat = platform_of_file(relf)
        if not plat:
            return None
        path = os.path.join(root, shard_of(entity), entity, plat, *parts)
    else:
        path = os.path.join(root, entity, *parts)
    return path if os.path.isfile(path) else None


class Handler(BaseHTTPRequestHandler):
    server_version = "abgen-dirserve/0.1"

    def _send(self, status, body, ctype="text/plain"):
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        cfg = self.server.cfg
        parts = [p for p in urllib.parse.urlparse(self.path).path.split("/") if p]
        if ".." in parts:
            return self._send(400, b"bad path")
        if len(parts) == 2 and parts[0] == "manifest" and parts[1].endswith(".json"):
            body = manifest_response(cfg["root"], cfg["layout"],
                                     parts[1][:-5], cfg["drops"])
            if body is None:
                return self._send(404, b"manifest not found")
            return self._send(200, body, "application/json")
        if len(parts) >= 3:
            path = payload_path(cfg["root"], cfg["layout"], parts[1],
                                "/".join(parts[2:]), cfg["drops"])
            if not path:
                return self._send(404, b"payload not found")
            with open(path, "rb") as f:
                return self._send(200, f.read(), "application/octet-stream")
        self._send(404, b"unknown route")

    def log_message(self, fmt, *args):
        if os.environ.get("DIRSERVE_VERBOSE"):
            super().log_message(fmt, *args)


def make_server(root, layout, port=0, drops=()):
    srv = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    srv.cfg = {"root": os.path.abspath(root), "layout": layout,
               "drops": set(drops)}
    return srv


def parse_drops(items):
    drops = set()
    for d in items:
        entity, _, fname = d.partition("/")
        if not entity or not fname:
            raise SystemExit(f"--drop wants <entity>/<file>, got {d!r}")
        drops.add((entity, fname))
    return drops


def main(argv=None):
    ap = argparse.ArgumentParser(prog="python3 -m abgencompare.dirserve",
                                 description=__doc__)
    ap.add_argument("--root", required=True)
    ap.add_argument("--port", type=int, default=5148)
    ap.add_argument("--layout", choices=("ref", "flat"), default="ref")
    ap.add_argument("--drop", action="append", default=[],
                    metavar="ENTITY/FILE")
    args = ap.parse_args(argv)
    if not os.path.isdir(args.root):
        raise SystemExit(f"--root {args.root} is not a directory")
    srv = make_server(args.root, args.layout, args.port, parse_drops(args.drop))
    print(f"dirserve: http://127.0.0.1:{srv.server_address[1]} "
          f"root={os.path.abspath(args.root)} layout={args.layout} "
          f"drops={len(srv.cfg['drops'])}", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
