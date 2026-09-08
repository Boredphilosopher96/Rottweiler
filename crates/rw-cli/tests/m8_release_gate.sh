#!/bin/sh
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
cd "$repo"
if [ "$#" -ne 2 ]; then
  echo "usage: m8_release_gate.sh VERIFIED_CANDIDATE MCP_FIXTURE_RECEIPT" >&2
  exit 2
fi
if [ "${ROTTWEILER_M8_FUNCTIONAL_ONLY:-0}" != 0 ]; then
  echo "native M8 qualification does not accept functional-only mode" >&2
  exit 2
fi
set -- --candidate "$1" --fixture-receipt "$2" --samples "${ROTTWEILER_M8_PERF_SAMPLES:-100}"
if [ -n "${ROTTWEILER_PERF_OUTPUT:-}" ]; then
  set -- "$@" --metrics-json "$ROTTWEILER_PERF_OUTPUT"
fi
# Python owns private copies, cancellation, physical closure and input fences.
exec python3 crates/rw-cli/tests/m8_release_gate.py "$@"
