#!/usr/bin/env bash
# Classify the files that differ between two source checkouts of the
# reference AWS CLI by what moving the reference across them takes.
#
# Usage: scripts/classify-reference-changes.sh OLD_SRC NEW_SRC
#
# Prints one line per changed file or per command whose help documents
# changed:
#   port <path>     a change that scripts/update-reference.sh does not carry
#                   over: awsr follows it only by porting it
#   help <command>  a command (or `help`, for the topics) whose example or
#                   topic documents changed, to compare its help pages
# Every other changed file is one that scripts/update-reference.sh
# regenerates awsr's data from (the botocore models and endpoints, the
# example and topic documents), one that only carries the version number,
# or one the CLI does not ship (the changelog, the tests, the CI).
set -euxo pipefail -o posix

old="$1"
new="$2"
version_of() { sed -n "s/^__version__ = .\(.*\).$/\1/p" "$1/awscli/__init__.py"; }
old_version="$(version_of "$old")"
new_version="$(version_of "$new")"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

LC_ALL=C diff -rq "$old" "$new" >"$tmp_dir/diff.txt" || [ "$?" = 1 ]
awk -v old="$old/" -v only="$tmp_dir/only.txt" '
  /^Files / { sub(/^Files /, ""); sub(/ and .* differ$/, ""); print substr($0, length(old) + 1); next }
  /^Only in / { sub(/^Only in /, ""); i = index($0, ": "); print substr($0, 1, i - 1) "/" substr($0, i + 2) >only; next }
  { print "unparsed: " $0 >"/dev/stderr"; exit 1 }
' "$tmp_dir/diff.txt" >"$tmp_dir/changed.txt"
touch "$tmp_dir/only.txt"
while IFS= read -r entry; do
  case "$entry" in
    "$old"/*) root="$old" ;;
    "$new"/*) root="$new" ;;
    *) exit 1 ;;
  esac
  (cd "$root" && find "${entry#"$root"/}" -type f) >>"$tmp_dir/changed.txt"
done <"$tmp_dir/only.txt"
sort -u -o "$tmp_dir/changed.txt" "$tmp_dir/changed.txt"

awk '
  $0 == "awscli/botocore/data/endpoints.json" || $0 == "awscli/botocore/data/partitions.json" { next }
  /^awscli\/botocore\/data\/[^\/]+\/[^\/]+\/(service-2|paginators-1|waiters-2)(\.sdk-extras)?\.json$/ { next }
  /^awscli\/botocore\/data\/[^\/]+\/[^\/]+\/endpoint-rule-set-1\.json$/ { next }
  /^awscli\/examples\/[^\/]+\/./ { split($0, p, "/"); print "help " p[3]; next }
  /^awscli\/topics\// { print "help help"; next }
  $0 == "awscli/__init__.py" || $0 == "configure" || $0 == "configure.ac" || $0 == "doc/source/conf.py" { print "version " $0; next }
  $0 == "CHANGELOG.rst" || /^\.changes\// || /^tests\// || /^\.github\// { next }
  { print "port " $0 }
' "$tmp_dir/changed.txt" >"$tmp_dir/classified.txt"

old_pattern="$(printf '%s' "$old_version" | sed 's/\./\\./g')"
while read -r kind path; do
  if [ "$kind" = version ]; then
    if [ -f "$old/$path" ] && [ -f "$new/$path" ] && sed "s/$old_pattern/$new_version/g" "$old/$path" | cmp -s - "$new/$path"; then
      continue
    fi
    kind=port
  fi
  echo "$kind $path"
done <"$tmp_dir/classified.txt" | sort -u
