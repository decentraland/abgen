#!/usr/bin/env bash

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

nix build --keep-going --no-link --log-format raw "${attrs[@]}" \
  || nix build --keep-going --no-link --print-build-logs "${attrs[@]}"
