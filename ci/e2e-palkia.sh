#!/usr/bin/env bash
# End-to-end conversion regression, held against a committed digest of every bundle.
#
# Converts one real world with the lambda binary exactly as the deployed pipeline invokes
# it, then hashes every bundle in the manifest and diffs that against ci/e2e/palkia.sha256.
# A byte of converter output that moves without someone meaning it to fails the job.
#
# The world is palkia.dcl.eth 0,0, which holds the **Pride Theme Genesis Plaza** — a real
# Genesis Plaza dressed for Pride. That is the reason it is the corpus and not some small
# fixture: it is a scene the whole platform actually serves, 347 glTFs and ~970 bundles per
# platform, carrying the full spread the converter has to get right — skinned rigs with
# clips, animated props without skins, standalone images, glTFs with embedded textures, and
# the deduplicated asset pool a plaza-sized scene brings with it.
#
# The entity id is PINNED rather than resolved from 0,0 every run. Entity ids are
# content-addressed, so a pinned one fixes the input forever and the digest stays meaningful;
# resolving live would let a redeploy of the world turn this into a random red build. The
# world *is* resolved anyway, as an advisory: if 0,0 has moved on, the job says so and keeps
# going, so the pin gets refreshed deliberately instead of silently drifting from reality.
#
# Two failure shapes, and the difference matters:
#   - a name appears or disappears  -> a digest moved, i.e. a recipe bump or a deps change.
#     Expected when you bumped one; the diff names exactly which bundles it moved.
#   - a name stays and its hash changes -> output changed at a key the CDN already serves,
#     which is the dangerous one: every client and every probe keeps the stale bytes. Either
#     the change was not meant to alter output, or it needs a recipe or AB_VERSION bump.
#
# Regenerate after an intended change, in the same commit as the change:
#   ci/e2e-palkia.sh --update
set -euo pipefail

ENTITY="bafkreigizvn772fwaihnjdlxuifgyainsn4ocmbfibswumdsfqyvr3d6ba"
WORLD="palkia.dcl.eth"
CONTENT_SERVER="https://worlds-content-server.decentraland.org"
REGISTRY="https://asset-bundle-registry-abgen.decentraland.org"

AB_VERSION="${AB_VERSION:-v1003}"
PLATFORMS="${PLATFORMS:-windows,mac}"
BIN="${ABGEN_LAMBDA_BIN:-target/release/abgen-lambda}"
GOLDEN="${GOLDEN:-ci/e2e/palkia.sha256}"

UPDATE=0
[ "${1:-}" = "--update" ] && UPDATE=1

[ -x "$BIN" ] || { echo "::error::abgen-lambda not found at $BIN (build it, or set ABGEN_LAMBDA_BIN)"; exit 2; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cat > "$work/event.json" <<EOF
{ "entityId": "$ENTITY", "contentServerUrl": "$CONTENT_SERVER" }
EOF

# Advisory only — never fails the job. A world redeploy changes the entity id, and the pin is
# what keeps this test deterministic, so drift is reported and left for a human to act on.
live="$(curl -sS -m 30 -X POST "$REGISTRY/entities/active?world_name=$WORLD" \
  -H 'Content-Type: application/json' --data '{"pointers":["0,0"]}' 2>/dev/null \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["id"])' 2>/dev/null || true)"
if [ -n "$live" ] && [ "$live" != "$ENTITY" ]; then
  echo "::warning::$WORLD 0,0 is now $live; this test is pinned to $ENTITY. Refresh the pin and the golden when you want the newer deployment covered."
fi

echo "converting $ENTITY ($WORLD 0,0) at $AB_VERSION for $PLATFORMS"
export AB_VERSION PLATFORMS
export OUT_ROOT="$work/out"
export ABGEN_CACHE_DIR="$work/cache"
export KEEP_OUTPUT=1
export CONTENT_SERVER_URL="$CONTENT_SERVER"
if ! "$BIN" --once "$work/event.json" > "$work/stdout.log" 2>&1; then
  echo "::error::conversion failed"
  tail -40 "$work/stdout.log"
  exit 1
fi
tail -6 "$work/stdout.log"

# The corpus is the authority on what was built; stdout is kept for the log and not parsed.
[ -d "$work/out/$ENTITY" ] || {
  echo "::error::no corpus at $work/out/$ENTITY — OUT_ROOT did not take effect"
  tail -20 "$work/stdout.log"
  exit 1
}

python3 - "$work" "$ENTITY" "$PLATFORMS" "$GOLDEN" "$UPDATE" <<'PY'
import hashlib, json, os, sys

work, entity, platforms, golden, update = sys.argv[1], sys.argv[2], sys.argv[3].split(","), sys.argv[4], sys.argv[5] == "1"

lines, recipes = [], None
for platform in platforms:
    manifest = json.load(open(f"{work}/out/{entity}/{platform}.manifest.json"))
    if manifest["exitCode"] != 0:
        print(f"::error::{platform} manifest reports exitCode {manifest['exitCode']}")
        sys.exit(1)
    # Every conversion this binary writes vouches for the whole recipe set; a manifest with
    # no block would mean the gate can no longer tell a stale bundle from a current one.
    if "recipes" not in manifest:
        print(f"::error::{platform} manifest has no recipes block")
        sys.exit(1)
    if recipes is None:
        recipes = manifest["recipes"]
    elif manifest["recipes"] != recipes:
        print(f"::error::{platform} recipes {manifest['recipes']} disagree with {recipes}")
        sys.exit(1)
    for name in manifest["files"]:
        if name == "dcl":
            continue
        path = f"{work}/out/{entity}/{platform}/{name}"
        if not os.path.isfile(path):
            print(f"::error::{platform} manifest lists {name}, which was not written")
            sys.exit(1)
        with open(path, "rb") as fh:
            lines.append(f"{hashlib.sha256(fh.read()).hexdigest()}  {platform}/{name}")

lines.sort()
header = [
    "# abgen e2e golden — palkia.dcl.eth 0,0, the Pride Theme Genesis Plaza",
    f"# entity {entity}, platforms {sys.argv[3]}",
    f"# recipes: {json.dumps(recipes, sort_keys=True)}",
    f"# {len(lines)} bundles; regenerate with ci/e2e-palkia.sh --update",
]
body = "\n".join(header + lines) + "\n"

if update:
    os.makedirs(os.path.dirname(golden), exist_ok=True)
    open(golden, "w").write(body)
    print(f"wrote {golden}: {len(lines)} bundles, recipes {json.dumps(recipes, sort_keys=True)}")
    sys.exit(0)

if not os.path.exists(golden):
    print(f"::error::{golden} is missing — generate it with ci/e2e-palkia.sh --update")
    sys.exit(1)

def parse(text):
    out = {}
    for line in text.splitlines():
        if line.startswith("#") or not line.strip():
            continue
        digest, name = line.split("  ", 1)
        out[name] = digest
    return out

want, got = parse(open(golden).read()), parse(body)
added = sorted(set(got) - set(want))
removed = sorted(set(want) - set(got))
changed = sorted(n for n in set(want) & set(got) if want[n] != got[n])

if not (added or removed or changed):
    print(f"ok: {len(got)} bundles match {golden} (recipes {json.dumps(recipes, sort_keys=True)})")
    sys.exit(0)

print(f"::error::conversion output drifted from {golden}: "
      f"{len(changed)} bundle(s) changed at an unchanged name, {len(added)} new name(s), {len(removed)} gone")
if changed:
    print("\nCHANGED BYTES AT AN UNCHANGED NAME — the CDN already serves these keys, so nothing")
    print("would pick the new bytes up. This needs a recipe bump, an AB_VERSION bump, or it was")
    print("not meant to change output at all:")
    for n in changed[:20]:
        print(f"  {n}\n    golden {want[n]}\n    built  {got[n]}")
    if len(changed) > 20:
        print(f"  … {len(changed) - 20} more")
for label, names in (("NEW NAMES", added), ("NAMES GONE", removed)):
    if names:
        print(f"\n{label} ({len(names)}):")
        for n in names[:20]:
            print(f"  {n}")
        if len(names) > 20:
            print(f"  … {len(names) - 20} more")
print("\nIf every one of these is intended, regenerate with: ci/e2e-palkia.sh --update")
sys.exit(1)
PY
