#!/usr/bin/env bash
set -euo pipefail

if [ $# -lt 2 ]; then
  echo "usage: bench/run.sh <awsr> <aws> [output.json]" >&2
  exit 2
fi

awsr=$(command -v "$1")
aws=$(command -v "$2")
out=${3:-bench-result.json}
dir=$(cd "$(dirname "$0")" && pwd)
port=${BENCH_PORT:-47651}

python3 "$dir/stub.py" "$port" &
stub=$!
trap 'kill $stub' EXIT
for _ in $(seq 50); do
  python3 -c "import socket; socket.create_connection(('127.0.0.1', $port))" 2>/dev/null && break
  sleep 0.1
done
kill -0 $stub

export AWS_ACCESS_KEY_ID=AKIDEXAMPLE AWS_SECRET_ACCESS_KEY=secret AWS_DEFAULT_REGION=us-east-1
export AWS_PAGER= AWS_EC2_METADATA_DISABLED=true AWS_CONFIG_FILE=/dev/null AWS_SHARED_CREDENTIALS_FILE=/dev/null
HOME=$(mktemp -d)
export HOME
endpoint=--endpoint-url=http://127.0.0.1:$port

commands=(
  "sts get-caller-identity $endpoint"
  "dynamodb list-tables $endpoint"
  "ec2 describe-regions $endpoint"
  "--version"
  "ec2 help"
)

for c in "${commands[@]}"; do
  case $c in
    --version | *help) ;;
    *)
      if ! cmp -s <($awsr $c 2>&1) <($aws $c 2>&1); then
        echo "outputs differ: $c" >&2
        diff <($aws $c 2>&1) <($awsr $c 2>&1) | head -20 >&2
        exit 1
      fi
      ;;
  esac
done
echo "awsr and aws print the same bytes for every API call" >&2

args=()
for c in "${commands[@]}"; do
  args+=(-n "awsr ${c%% --endpoint-url*}" "$awsr $c" -n "aws ${c%% --endpoint-url*}" "$aws $c")
done
hyperfine -N --warmup 5 --min-runs 30 --style basic --export-json "$out" "${args[@]}"
