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

# --max-jobs 2: the full check set holds several workspace-scale compiles;
# unbounded derivation parallelism OOM-kills the 16 GB runners. Two at a
# time bounds memory while keeping the cores saturated.
nix build --keep-going --no-link --log-format raw --max-jobs 2 "${attrs[@]}" \
  || nix build --keep-going --no-link --print-build-logs --max-jobs 2 "${attrs[@]}"

# A test check that executed zero tests is a silent no-op (doCheck=false
# reached the derivation once and nobody noticed for weeks) — make that
# state red forever: the built check's log must show a nonzero test count.
assert_ran_tests() {
  local attr="$1" pattern="$2"
  # CARGO_TERM_COLOR=always wraps the summary in ANSI escapes; strip them
  # or the pattern can never match a CI log.
  nix log "$attr" | sed $'s/\x1b\\[[0-9;]*[A-Za-z]//g' | grep -qE "$pattern" \
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
