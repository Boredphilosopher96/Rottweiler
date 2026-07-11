#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
cd "$repo"

target=$(mktemp -d "${TMPDIR:-/tmp}/rottweiler-m8-target.XXXXXX")
artifacts=$(mktemp -d "${TMPDIR:-/tmp}/rottweiler-m8-artifacts.XXXXXX")
trap 'rm -rf "$target" "$artifacts"' EXIT HUP INT TERM

export CARGO_TARGET_DIR="$target"
export CARGO_PROFILE_RELEASE_DEBUG=0
export ROTTWEILER_CREDENTIAL_BACKEND=file

scripts/cargo-release.sh build --locked --release \
  -p rw-cli --bin rw \
  -p rw-mcp --features rw-mcp/test-support --bin rw-mcp-fixture
release_dir=$(scripts/cargo-release.sh artifact-dir)

cp "$release_dir/rw" "$artifacts/rw"
cp "$release_dir/rw-mcp-fixture" "$artifacts/rw-mcp-fixture"
rm -rf "$target"

set -- \
  --rw "$artifacts/rw" \
  --fixture "$artifacts/rw-mcp-fixture" \
  --samples "${ROTTWEILER_M8_PERF_SAMPLES:-100}"

if [ "${ROTTWEILER_M8_FUNCTIONAL_ONLY:-0}" = 1 ]; then
  set -- "$@" --functional-only
fi
if [ -n "${ROTTWEILER_PERF_OUTPUT:-}" ]; then
  set -- "$@" --metrics-json "$ROTTWEILER_PERF_OUTPUT"
fi

python3 crates/rw-cli/tests/m8_release_gate.py "$@"
