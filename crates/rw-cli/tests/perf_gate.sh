#!/bin/sh
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
exec python3 "$repo/crates/rw-cli/tests/perf_gate.py" "$@"
