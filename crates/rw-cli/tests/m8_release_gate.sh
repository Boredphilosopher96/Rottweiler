#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
cd "$repo"

if [ "$#" -ne 2 ]; then
  echo "usage: m8_release_gate.sh ENGINE_EXECUTABLE MCP_FIXTURE_EXECUTABLE" >&2
  exit 2
fi
engine=$1
fixture=$2
artifacts=$(mktemp -d "${TMPDIR:-/tmp}/rottweiler-m8-artifacts.XXXXXX")
trap 'rm -rf "$artifacts"' EXIT HUP INT TERM

# The caller prepares both artifacts before measurement and host conditioning.
export ROTTWEILER_CREDENTIAL_BACKEND=file
cp "$engine" "$artifacts/rw"
cp "$fixture" "$artifacts/rw-mcp-fixture"

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
