#!/usr/bin/env bash
# Compare help pages of awsr and the reference AWS CLI (stdout, stderr,
# exit code) after turning the name `awsr` (`AWSR` in the page header) back into `aws`: byte for byte,
# or, when the longer name moved the line breaks of a paragraph, with the
# bold and underline codes, runs of whitespace, and hyphenations at line
# ends taken out.
#
# Usage: scripts/compare-help.sh LIST [JOBS]
#   LIST: help command lines, one per line (scripts/list-help-pages.py
#   prints every page). Needs the devShell ($AWSR_REFERENCE_AWS, groff)
#   and a built target/release/awsr (or $AWSR).
#
# Prints one `DIFF <command line>` per mismatch, one `WRAP <command line>`
# per page that matches only with the line breaks taken out, and the counts.
# Both run with an empty HOME and without AWS_* or PAGER settings, and with
# stdout not a terminal, so the pager passes the page through.
set -euxo pipefail -o posix

list="$1"
jobs="${2:-8}"
awsr="${AWSR:-$PWD/target/release/awsr}"
ref="${AWSR_REFERENCE_AWS:?run inside nix develop}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/home"

flow() {
  sed 's/\x1b\[[0-9;]*m//g' | tr -s '[:space:]' ' ' | sed 's/\([[:alpha:]]\)- /\1/g'
}

compare() {
  local line="$1" out
  out="$(mktemp -p "$work")"
  # shellcheck disable=SC2086
  env -i PATH="$PATH" HOME="$work/home" TERM=dumb "$ref" $line >"$out.ref.out" 2>"$out.ref.err" </dev/null && echo 0 >"$out.ref.rc" || echo $? >"$out.ref.rc"
  # shellcheck disable=SC2086
  env -i PATH="$PATH" HOME="$work/home" TERM=dumb "$awsr" $line >"$out.awsr.out" 2>"$out.awsr.err" </dev/null && echo 0 >"$out.awsr.rc" || echo $? >"$out.awsr.rc"
  sed -i 's/awsr\([^[:alnum:]_]\|$\)/aws\1/g; s/AWSR\([^[:alnum:]_]\|$\)/AWS\1/g' "$out.awsr.out" "$out.awsr.err"
  if ! cmp -s "$out.ref.err" "$out.awsr.err" || ! cmp -s "$out.ref.rc" "$out.awsr.rc"; then
    echo "DIFF $line"
  elif cmp -s "$out.ref.out" "$out.awsr.out"; then
    echo "OK $line"
  elif cmp -s <(flow <"$out.ref.out") <(flow <"$out.awsr.out"); then
    echo "WRAP $line"
  else
    echo "DIFF $line"
  fi
  rm -f "$out" "$out".*
}
export -f flow compare
export work awsr ref

grep -v '^\s*$' "$list" | xargs -P "$jobs" -I{} bash -c 'compare "$@"' _ {} >"$work/results"
grep '^\(DIFF\|WRAP\) ' "$work/results" | sort || true
ok="$(grep -c '^OK' "$work/results" || true)"
wrap="$(grep -c '^WRAP' "$work/results" || true)"
total="$(wc -l <"$work/results")"
echo "match: $ok/$total, match with the line breaks taken out: $wrap"
