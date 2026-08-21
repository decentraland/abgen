#!/usr/bin/env bash

set -euo pipefail

system="${1:?usage: nix-checks.sh <system> [attr...]}"
shift || true

if [ $# -gt 0 ]; then
  names="$(printf '%s\n' "$@")"
else
  names="$(nix eval --raw ".#checks.${system}" \
    --apply 'checks: builtins.concatStringsSep "\n" (builtins.attrNames checks)')"
fi

[ -n "$names" ] || { echo "no checks for ${system}" >&2; exit 1; }

attrs=()
while IFS= read -r name; do
  attrs+=(".#checks.${system}.${name}")
done <<<"$names"

printf 'building %s check(s):\n' "${#attrs[@]}"
printf '  %s\n' "${attrs[@]}"

# --max-jobs 2: unbounded drv parallelism OOM-kills the 16 GB runners
nix build --keep-going --no-link --log-format raw --max-jobs 2 "${attrs[@]}" \
  || nix build --keep-going --no-link --print-build-logs --max-jobs 2 "${attrs[@]}"
