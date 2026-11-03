#!/usr/bin/env bash
# Usage: scripts/vendor-models.sh <aws-cli source checkout at the reference tag>
# Needs the devShell (jq, zstd, and a Python that imports the checkout).
set -euo pipefail

src="$1"
data="$src/awscli/botocore/data"
out="models"

rm -rf "$out"
mkdir -p "$out/services"
tmp_dir="$(mktemp -d)"
null_file="$tmp_dir/null.json"
echo null > "$null_file"
trap 'rm -rf "$tmp_dir"' EXIT

for dir in "$data"/*/; do
  service="$(basename "$dir")"
  version="$(find "$dir" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | sort | tail -n 1)"
  [ -n "$version" ] || continue
  vdir="$dir$version"
  [ -f "$vdir/service-2.json" ] || continue
  path_or_null() { if [ -f "$vdir/$1" ]; then echo "$vdir/$1"; else echo "$null_file"; fi; }
  with_extras() {
    local base="$vdir/$1.json" extras="$vdir/$1.sdk-extras.json" merged="$tmp_dir/$service.$1.json"
    if [ ! -f "$base" ]; then echo "$null_file"; return; fi
    if [ -f "$extras" ]; then
      jq -c --slurpfile e "$extras" '. * ($e[0].merge // {})' "$base" > "$merged"
      echo "$merged"
    else
      echo "$base"
    fi
  }
  jq -c -n \
    --arg version "$version" \
    --slurpfile service "$(with_extras service-2)" \
    --slurpfile paginators "$(with_extras paginators-1)" \
    --slurpfile waiters "$(with_extras waiters-2)" \
    --slurpfile ruleset "$(path_or_null endpoint-rule-set-1.json)" \
    '{api_version: $version, service: $service[0], paginators: ($paginators[0].pagination // null), waiters: ($waiters[0].waiters // null), ruleset: $ruleset[0]}' \
    | zstd -19 -q -o "$out/services/$service.json.zst"
done

for f in partitions.json endpoints.json; do
  jq -c . "$data/$f" | zstd -19 -q -o "$out/$f.zst"
done
jq -c . "$src/awscli/data/cli.json" | zstd -19 -q -o "$out/cli.json.zst"
sed -n "s/^__version__ = .\(.*\).$/\1/p" "$src/awscli/__init__.py" > "$out/VERSION"

scripts_dir="$(dirname "$0")"
python3 "$scripts_dir/vendor-docs.py" "$src" "$tmp_dir/docs"
PYTHONPATH="$src" python3 "$scripts_dir/gen-help-data.py" "$tmp_dir/help"
mkdir -p "$out/examples" "$out/help"
for f in "$tmp_dir"/docs/examples/*.json; do
  zstd -19 -q -o "$out/examples/$(basename "$f").zst" "$f"
done
zstd -19 -q -o "$out/topics.json.zst" "$tmp_dir/docs/topics.json"
for f in "$tmp_dir"/help/*.json; do
  zstd -19 -q -o "$out/help/$(basename "$f").zst" "$f"
done
