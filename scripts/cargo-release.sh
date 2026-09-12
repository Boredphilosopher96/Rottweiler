#!/bin/sh
set -eu

# Native release optimization is platform-specific: opt-level 3 on macOS and
# s on Linux. scripts/native_profile.py owns the exact settings consumed by
# Cargo, candidate receipts, and size/performance gates.
# Official packages and performance evidence are native-only. Force Cargo's
# host target so user or ancestor build.target configuration cannot redirect
# output while a gate inspects or packages a stale host-path executable.
if [ -n "${CARGO_BUILD_TARGET:-}" ]; then
  echo "cargo-release: CARGO_BUILD_TARGET is unsupported for native release builds" >&2
  exit 2
fi
has_release=0
for argument in "$@"; do
  case "$argument" in
    --release) has_release=1 ;;
    --target|--target=*)
      echo "cargo-release: --target is unsupported for native release builds" >&2
      exit 2
      ;;
    --target-dir|--target-dir=*)
      echo "cargo-release: --target-dir is owned by the native release wrapper" >&2
      exit 2
      ;;
  esac
done
target=$(rustc -vV | sed -n 's/^host: //p')

case "$target" in
  *-apple-darwin|*-linux-gnu|*-linux-musl) ;;
  *)
    echo "cargo-release: unsupported release target: $target" >&2
    exit 2
    ;;
esac

target_root=${CARGO_TARGET_DIR:-target}
case "$target_root" in
  /*) ;;
  *) target_root=$PWD/$target_root ;;
esac
export CARGO_TARGET_DIR=$target_root
if [ "${1:-}" = artifact-dir ]; then
  if [ "$#" -ne 1 ]; then
    echo "usage: cargo-release.sh artifact-dir" >&2
    exit 2
  fi
  printf '%s/%s/release\n' "$target_root" "$target"
  exit 0
fi
if [ "${1:-}" != build ]; then
  echo "usage: cargo-release.sh build [cargo build options]" >&2
  exit 2
fi
shift
if [ "$has_release" != 1 ]; then
  echo "cargo-release: native builds require --release" >&2
  exit 2
fi

# One owner supplies the exact profile to Cargo, receipts, and diagnostics.
exec python3 "$(dirname "$0")/native_profile.py" "$target" build --target "$target" --target-dir "$target_root" "$@"
