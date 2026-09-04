#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
cd "$repo"

export ROTTWEILER_CREDENTIAL_BACKEND=file
export CARGO_PROFILE_RELEASE_DEBUG=0

scripts/cargo-release.sh build --locked --release -p rw-cli
release_dir=$(scripts/cargo-release.sh artifact-dir)
(cd packages/tui && bun run build)

set -- \
  --repo "$repo" \
  --rw "$release_dir/rw" \
  --tui "$repo/packages/tui/dist/rottweiler-tui" \
  --samples "${ROTTWEILER_M4_PERF_SAMPLES:-100}"

if [ "${ROTTWEILER_M4_SKIP_PERFORMANCE:-0}" = 1 ]; then
  set -- "$@" --skip-performance
fi
if [ "${ROTTWEILER_M4_SKIP_SUPERVISOR:-0}" = 1 ]; then
  set -- "$@" --skip-supervisor
fi
if [ "${ROTTWEILER_M4_SKIP_SHELL:-0}" = 1 ]; then
  set -- "$@" --skip-shell
fi
if [ -n "${ROTTWEILER_M4_SSH_LOOPBACK_HOST:-}" ]; then
  set -- "$@" --ssh-loopback "$ROTTWEILER_M4_SSH_LOOPBACK_HOST"
fi
if [ -n "${ROTTWEILER_PERF_OUTPUT:-}" ]; then
  set -- "$@" --metrics-json "$ROTTWEILER_PERF_OUTPUT"
fi

if [ -n "${ROTTWEILER_M4_EVIDENCE_OUTPUT:-}" ]; then
  set -- "$@" --evidence-json "$ROTTWEILER_M4_EVIDENCE_OUTPUT"
fi

exec python3 crates/rw-cli/tests/m4_release_gate.py "$@"
