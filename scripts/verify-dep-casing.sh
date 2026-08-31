#!/usr/bin/env bash
# End-to-end local check of the per-platform CDN casing contract on fused
# corpus output (PR #110): converts one entity the way the backfill does
# (--cdn-layout --platform windows,mac) and asserts, per platform, that
#
#   1. every produced bundle file name carries the content hash with the
#      contract casing — fully lowercase on mac, the catalyst-original
#      casing on windows — and
#   2. every dependency entry embedded in a bundle's metadata.json names a
#      file that exists in the same platform directory BYTE-EXACTLY (the
#      client fetches these entries verbatim from a case-sensitive CDN).
#
# Default entity: urn:decentraland:ethereum:collections-v1:exclusive_masks:tropical_mask
# — an old wearable whose GLB references AvatarWearables_TX.png externally,
# so it exercises mixed-case Qm hashes both as upload names and as deps.
#
# Usage: scripts/verify-dep-casing.sh [entity-id]
#   CONTENT_URL   content server (default https://peer.decentraland.org/content)
#   WORK          scratch dir (default target/verify-dep-casing; wiped per run)
set -euo pipefail

ENTITY_ID="${1:-QmRixpiTwgR371sZ6KZF9ycio3Y2bLZMdQRXHixhUxSdA6}"
CONTENT_URL="${CONTENT_URL:-https://peer.decentraland.org/content}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${WORK:-$ROOT/target/verify-dep-casing}"

echo "== building abgen-corpus + metadump"
cargo build --release --manifest-path "$ROOT/Cargo.toml" -p abgen \
  --bin abgen-corpus --example metadump

rm -rf "$WORK/out"
mkdir -p "$WORK"
echo "$ENTITY_ID" > "$WORK/ids.txt"

echo "== converting $ENTITY_ID (fused windows,mac, cdn layout — backfill shape)"
"$ROOT/target/release/abgen-corpus" --entity-ids "$WORK/ids.txt" "$WORK/out" \
  --platform windows,mac --cdn-layout --fetch-missing \
  --content-server-url "$CONTENT_URL" --content-dir "$WORK/content" \
  --ab-version v1002

echo "== fetching entity content table"
curl -fsS "$CONTENT_URL/contents/$ENTITY_ID" -o "$WORK/entity.json"

echo "== dumping embedded metadata"
find "$WORK/out/$ENTITY_ID" -type f ! -name '*.json' ! -name '*.br' \
  -exec "$ROOT/target/release/examples/metadump" {} + > "$WORK/metadump.tsv"

echo "== verifying"
python3 - "$ENTITY_ID" "$WORK" <<'PY'
import json, os, sys

entity_id, work = sys.argv[1], sys.argv[2]
entity = json.load(open(f"{work}/entity.json"))
hashes = [c["hash"] for c in entity["content"]]
by_lower = {h.lower(): h for h in hashes}
out_root = f"{work}/out/{entity_id}"

failures, deps_seen = [], 0

def hash_of(name, platform):
    stem = name.removesuffix(".br").removesuffix(f"_{platform}")
    return stem.split("_")[0]

for platform in ("windows", "mac"):
    pdir = f"{out_root}/{platform}"
    files = set(os.listdir(pdir))  # exact names: os.path.exists lies on
                                   # case-insensitive filesystems (APFS)
    bundles = sorted(f for f in files if not f.endswith(".json"))
    if not bundles:
        failures.append(f"{platform}: no bundles produced")
        continue

    for name in bundles:
        seg = hash_of(name, platform)
        original = by_lower.get(seg.lower())
        if original is None:
            failures.append(f"{platform}/{name}: hash segment {seg!r} not in entity content")
            continue
        if platform == "mac":
            if name != name.lower():
                failures.append(f"mac/{name}: upload name must be fully lowercase")
        elif seg != original:
            failures.append(
                f"windows/{name}: hash segment {seg!r} must keep catalyst casing {original!r}")

for line in open(f"{work}/metadump.tsv"):
    path, script = line.rstrip("\n").split("\t", 1)
    if script == "-":
        continue
    platform = os.path.basename(os.path.dirname(path))
    pdir = f"{out_root}/{platform}"
    files = set(os.listdir(pdir))
    for dep in json.loads(script).get("dependencies", []):
        if dep.startswith("dcl/"):
            continue  # shader bundles, resolved client-side
        deps_seen += 1
        if dep not in files:
            close = [f for f in files if f.lower() == dep.lower()]
            failures.append(
                f"{platform}/{os.path.basename(path)}: dep {dep!r} has no byte-exact file"
                + (f" (casing mismatch with {close[0]!r})" if close else ""))
        seg = hash_of(dep, platform)
        original = by_lower.get(seg.lower())
        if platform == "mac" and dep != dep.lower():
            failures.append(f"mac dep {dep!r} must be fully lowercase")
        if platform == "windows" and original is not None and seg != original:
            failures.append(f"windows dep {dep!r} must keep catalyst casing {original!r}")

if deps_seen == 0:
    failures.append("no metadata dependencies found in any bundle — "
                    "the check exercised nothing (wrong entity?)")

if failures:
    print(f"FAIL ({len(failures)}):")
    for f in failures:
        print(f"  - {f}")
    sys.exit(1)
print(f"PASS: upload names and {deps_seen} embedded dep(s) honor the casing contract "
      f"on both platforms")
PY
