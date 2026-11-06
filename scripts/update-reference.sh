#!/usr/bin/env bash
# Move the reference AWS CLI to VERSION: pin its source and Linux installer
# in flake.nix, regenerate models/ from it, and regenerate the goldens that
# are derived from its models and help documents.
#
# Usage: scripts/update-reference.sh VERSION
#   Run outside the devShell (it enters the devShell of the new pin).
set -euo pipefail

version="$1"
current="$(cat models/VERSION)"

sri() { nix hash convert --hash-algo sha256 --to sri "$1"; }
src_hash="$(sri "$(nix-prefetch-url --unpack "https://github.com/aws/aws-cli/archive/refs/tags/$version.tar.gz" 2>/dev/null)")"
exe_hash="$(sri "$(nix-prefetch-url "https://awscli.amazonaws.com/awscli-exe-linux-x86_64-$version.zip" 2>/dev/null)")"

sed -i \
  -e "s/rev = \"$current\"/rev = \"$version\"/" \
  -e "s/version = \"$current\"/version = \"$version\"/" \
  flake.nix
awk -v src="$src_hash" -v exe="$exe_hash" '
  /fetchFromGitHub/ { in_src = 1 }
  /fetchurl/ { in_exe = 1 }
  in_src && /hash = / { sub(/"sha256-[^"]*"/, "\"" src "\""); in_src = 0 }
  in_exe && /hash = / { sub(/"sha256-[^"]*"/, "\"" exe "\""); in_exe = 0 }
  { print }
' flake.nix >flake.nix.new
mv flake.nix.new flake.nix

nix develop --command bash -euo pipefail -c '
  [ "$(aws --version | cut -d" " -f1)" = "aws-cli/'"$version"'" ]
  scripts/vendor-models.sh "$AWSR_REFERENCE_SRC"
  cargo update --workspace
  home="$(mktemp -d)"
  HOME="$home" PYTHONPATH="$AWSR_REFERENCE_SRC" python3 src/cli/testdata/gen_tables.py >src/cli/tables.rs
  HOME="$home" PYTHONPATH="$AWSR_REFERENCE_SRC" python3 src/cli/testdata/gen_fingerprints.py >src/cli/fingerprints.rs
  HOME="$home" PYTHONPATH="$AWSR_REFERENCE_SRC" python3 src/help/testdata/gen_documents.py src/help/testdata
  PYTHONPATH="$AWSR_REFERENCE_SRC" python3 src/testdata/gen_waiter_descriptions.py >src/testdata/waiter-descriptions.json
  nix fmt
'
