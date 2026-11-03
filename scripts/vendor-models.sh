#!/usr/bin/env bash
# Usage: scripts/vendor-models.sh <aws-cli source checkout at the reference tag>
set -euo pipefail

src="$1"
data="$src/awscli/botocore/data"
out="models"

rm -rf "$out"
mkdir -p "$out/services"
null_file="$(mktemp)"
echo null > "$null_file"
trap 'rm -f "$null_file"' EXIT

for dir in "$data"/*/; do
  service="$(basename "$dir")"
  version="$(find "$dir" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | sort | tail -n 1)"
  [ -n "$version" ] || continue
  vdir="$dir$version"
  [ -f "$vdir/service-2.json" ] || continue
  path_or_null() { if [ -f "$vdir/$1" ]; then echo "$vdir/$1"; else echo "$null_file"; fi; }
  jq -c -n \
    --arg version "$version" \
    --slurpfile service "$vdir/service-2.json" \
    --slurpfile paginators "$(path_or_null paginators-1.json)" \
    --slurpfile waiters "$(path_or_null waiters-2.json)" \
    --slurpfile ruleset "$(path_or_null endpoint-rule-set-1.json)" \
    '{api_version: $version, service: $service[0], paginators: ($paginators[0].pagination // null), waiters: ($waiters[0].waiters // null), ruleset: $ruleset[0]}' \
    | zstd -19 -q -o "$out/services/$service.json.zst"
done

for f in partitions.json endpoints.json; do
  jq -c . "$data/$f" | zstd -19 -q -o "$out/$f.zst"
done
jq -c . "$src/awscli/data/cli.json" | zstd -19 -q -o "$out/cli.json.zst"
sed -n "s/^__version__ = .\(.*\).$/\1/p" "$src/awscli/__init__.py" > "$out/VERSION"
