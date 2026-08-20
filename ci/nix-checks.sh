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

# A test check that executed zero tests is a silent no-op (doCheck=false
# reached the derivation once and nobody noticed for weeks) — make that
# state red forever: the built check's log must show a nonzero test count.
assert_ran_tests() {
  local attr="$1" pattern="$2"
  grep -qE "$pattern" <(nix log "$attr") \
    || { echo "$attr built green but ran no tests ($pattern not in its log)" >&2; exit 1; }
}
while IFS= read -r name; do
  case "$name" in
    nextest) assert_ran_tests ".#checks.${system}.${name}" \
      'Summary \[.*\] +[1-9][0-9]* tests run' ;;
    lambda-tests) assert_ran_tests ".#checks.${system}.${name}" \
      'test result: ok\. [1-9]' ;;
  esac
done <<<"$names"
