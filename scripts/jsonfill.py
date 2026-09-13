"""JSON writer for committed fixtures: a container whose compact form fits in WIDTH columns stays
on one line, anything longer is split one child per line.

WIDTH is set so that a manifest or descriptor record fits on one line, which is the point of the
format: at 250 nearly every record wrapped across five to seven lines and the fixtures were four
times taller than the data they carry."""
import json

WIDTH = 1000


def dumps_fill(obj, indent=0):
    flat = json.dumps(obj, separators=(",", ":"), ensure_ascii=False)
    if not isinstance(obj, (dict, list)) or not obj or len(flat) + indent <= WIDTH:
        return flat
    pad = " " * (indent + 2)
    if isinstance(obj, list):
        rows = [pad + dumps_fill(v, indent + 2) for v in obj]
    else:
        rows = [pad + json.dumps(k, ensure_ascii=False) + ":" + dumps_fill(v, indent + 2) for k, v in obj.items()]
    open_, close_ = ("[", "]") if isinstance(obj, list) else ("{", "}")
    return open_ + "\n" + ",\n".join(rows) + "\n" + " " * indent + close_


def dump_fill(path, obj):
    with open(path, "w", encoding="utf-8") as f:
        f.write(dumps_fill(obj) + "\n")
