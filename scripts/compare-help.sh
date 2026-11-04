#!/usr/bin/env bash
# Compare help pages of awsr and the reference AWS CLI byte for byte
# (stdout, stderr, exit code).
#
# Usage: scripts/compare-help.sh LIST [JOBS]
#   LIST: help command lines, one per line (scripts/list-help-pages.py
#   prints every page). Needs the devShell ($AWSR_REFERENCE_AWS, groff)
#   and a built target/release/awsr (or $AWSR).
#
# Prints one `DIFF <command line>` per mismatch and the match counts.
# Both run with an empty HOME and without AWS_* or PAGER settings, and with
# stdout not a terminal, so the pager passes the page through.
set -euo pipefail

list="$1"
jobs="${2:-8}"
awsr="${AWSR:-$PWD/target/release/awsr}"
ref="${AWSR_REFERENCE_AWS:?run inside nix develop}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/home"

compare() {
  local line="$1" out
  out="$(mktemp -p "$work")"
  # shellcheck disable=SC2086
  env -i PATH="$PATH" HOME="$work/home" TERM=dumb "$ref" $line >"$out.ref.out" 2>"$out.ref.err" </dev/null && echo 0 >"$out.ref.rc" || echo $? >"$out.ref.rc"
  # shellcheck disable=SC2086
  env -i PATH="$PATH" HOME="$work/home" TERM=dumb "$awsr" $line >"$out.awsr.out" 2>"$out.awsr.err" </dev/null && echo 0 >"$out.awsr.rc" || echo $? >"$out.awsr.rc"
  if cmp -s "$out.ref.out" "$out.awsr.out" && cmp -s "$out.ref.err" "$out.awsr.err" && cmp -s "$out.ref.rc" "$out.awsr.rc"; then
    echo "OK $line"
  else
    echo "DIFF $line"
  fi
  rm -f "$out" "$out".*
}
export -f compare
export work awsr ref

grep -v '^\s*$' "$list" | xargs -P "$jobs" -I{} bash -c 'compare "$@"' _ {} >"$work/results"
grep '^DIFF' "$work/results" | sort || true
ok="$(grep -c '^OK' "$work/results" || true)"
total="$(wc -l <"$work/results")"
echo "match: $ok/$total"
