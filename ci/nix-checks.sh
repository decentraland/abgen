#!/usr/bin/env bash

# Build every flake check the given system carries. The per-arch split
# lives in nix/checks.nix (arch-independent checks exist only on
# x86_64-linux), so this script never hardcodes check names.

set -euo pipefail

system="${1:?usage: nix-checks.sh <system>}"

names="$(nix eval --raw ".#checks.${system}" \
  --apply 'checks: builtins.concatStringsSep "\n" (builtins.attrNames checks)')"

[ -n "$names" ] || { echo "no checks for ${system}" >&2; exit 1; }

attrs=()
while IFS= read -r name; do
  attrs+=(".#checks.${system}.${name}")
done <<<"$names"

printf 'building %s check(s):\n' "${#attrs[@]}"
printf '  %s\n' "${attrs[@]}"

# Quiet first: -L streams every check's full build log into the runner
# log even when green. On failure, rerun with logs — only the failed
# derivations rebuild, everything else is a store hit.
nix build --keep-going --no-link "${attrs[@]}" \
  || nix build --keep-going --no-link --print-build-logs "${attrs[@]}"
