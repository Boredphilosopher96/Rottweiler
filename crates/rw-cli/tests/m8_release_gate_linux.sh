#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
# This pinned Docker Official Image is the canonical Linux M8 sandbox and
# protocol-performance environment. Host-native headless/M4 gates still cover
# the release runner and archive outside this container.
image=${ROTTWEILER_LINUX_M8_IMAGE:-docker.io/library/rust:1.97.1-bookworm@sha256:77fac8b98f9f46062bb680b6d25d5bcaabfc400143952ebc572e924bcbedc3fa}
cargo_volume=${ROTTWEILER_LINUX_CARGO_VOLUME:-rottweiler-linux-cargo}
container="rottweiler-m8-${PPID}-$$"
m8_tmpfs_size=${ROTTWEILER_LINUX_M8_TMPFS_SIZE:-3g}

if [ "${ROTTWEILER_M8_FUNCTIONAL_ONLY:-0}" = 1 ] && \
  printenv ROTTWEILER_PERF_OUTPUT >/dev/null 2>&1
then
  echo "ROTTWEILER_PERF_OUTPUT requires the complete M8 performance gate" >&2
  exit 2
fi

if printenv ROTTWEILER_PERF_OUTPUT >/dev/null 2>&1; then
  case $ROTTWEILER_PERF_OUTPUT in
    /*) metrics=$ROTTWEILER_PERF_OUTPUT ;;
    *) metrics=$repo/$ROTTWEILER_PERF_OUTPUT ;;
  esac
  supplied_metrics_parent=$(dirname -- "$metrics")
  if [ ! -d "$supplied_metrics_parent" ]; then
    echo "ROTTWEILER_PERF_OUTPUT parent directory must already exist" >&2
    exit 2
  fi
  metrics_parent=$(CDPATH= cd -- "$supplied_metrics_parent" && pwd -P)
  case $metrics_parent in
    "$repo"|"$repo"/*) ;;
    *)
      echo "ROTTWEILER_PERF_OUTPUT must remain inside the bound repository" >&2
      exit 2
      ;;
  esac
fi

cleanup() {
  docker rm -f "$container" >/dev/null 2>&1 || :
}
trap cleanup EXIT HUP INT TERM

docker volume create "$cargo_volume" >/dev/null

set -- docker run --rm --privileged \
  --name "$container" \
  --mount "type=bind,source=$repo,target=$repo" \
  --mount "type=volume,source=$cargo_volume,target=/usr/local/cargo/registry" \
  --tmpfs "/m8-work:rw,exec,size=$m8_tmpfs_size" \
  --workdir "$repo" \
  --env TMPDIR=/m8-work \
  --env CARGO_INCREMENTAL=0 \
  --env CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}" \
  --env ROTTWEILER_CREDENTIAL_BACKEND=file \
  --env "ROTTWEILER_HOST_UID=$(id -u)" \
  --env "ROTTWEILER_HOST_GID=$(id -g)"

# Forward only variables that are actually set.
for variable in \
  ROTTWEILER_M8_PERF_SAMPLES \
  ROTTWEILER_M8_FUNCTIONAL_ONLY \
  ROTTWEILER_PERF_OUTPUT \
  ROTTWEILER_UPDATE_BASE_URL \
  ROTTWEILER_UPDATE_ROOT_KEYS_JSON \
  ROTTWEILER_UPDATE_ROOT_THRESHOLD \
  ROTTWEILER_UPDATE_ROOT_VERSION
do
  if printenv "$variable" >/dev/null 2>&1; then
    set -- "$@" --env "$variable"
  fi
done

set -- "$@" "$image" sh -eu -c '
  status=0
  if [ "${ROTTWEILER_M8_FUNCTIONAL_ONLY:-0}" = 1 ]; then
    if ! command -v ld.gold >/dev/null 2>&1; then
      echo "canonical Linux M8 functional linker /usr/bin/ld.gold is unavailable" >&2
      exit 2
    fi
    export CARGO_TARGET_DIR=/m8-work/functional-target
    export CARGO_PROFILE_DEV_DEBUG=0
    export CARGO_PROFILE_DEV_INCREMENTAL=false
    export RUSTFLAGS="-C link-arg=-fuse-ld=gold"
    cargo build --locked \
      -p rw-cli --bin rw \
      -p rw-mcp --features rw-mcp/test-support --bin rw-mcp-fixture || status=$?
    if [ "$status" -eq 0 ]; then
      mkdir -m 700 /m8-work/functional-artifacts
      cp "$CARGO_TARGET_DIR/debug/rw" /m8-work/functional-artifacts/rw
      cp "$CARGO_TARGET_DIR/debug/rw-mcp-fixture" \
        /m8-work/functional-artifacts/rw-mcp-fixture
      rm -rf "$CARGO_TARGET_DIR"
      python3 crates/rw-cli/tests/m8_release_gate.py \
        --rw /m8-work/functional-artifacts/rw \
        --fixture /m8-work/functional-artifacts/rw-mcp-fixture \
        --samples "${ROTTWEILER_M8_PERF_SAMPLES:-1}" \
        --functional-only || status=$?
    fi
  else
    export CARGO_TARGET_DIR=/m8-work/release-target
    export CARGO_PROFILE_RELEASE_DEBUG=0
    scripts/cargo-release.sh build --locked --release -p rw-cli --bin rw || status=$?
    if [ "$status" -eq 0 ]; then
      release_dir=$(scripts/cargo-release.sh artifact-dir)
      crates/rw-cli/tests/m8_release_gate.sh "$release_dir/rw" || status=$?
    fi
  fi
  if [ -n "${ROTTWEILER_PERF_OUTPUT:-}" ] && [ -e "$ROTTWEILER_PERF_OUTPUT" ]; then
    chown "$ROTTWEILER_HOST_UID:$ROTTWEILER_HOST_GID" "$ROTTWEILER_PERF_OUTPUT"
  fi
  exit "$status"
'

"$@"
