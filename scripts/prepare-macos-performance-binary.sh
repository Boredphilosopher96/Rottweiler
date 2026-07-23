#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo"

if [ "$(uname -s)" != Darwin ]; then
  echo "prepare-macos-performance-binary: macOS is required" >&2
  exit 2
fi
if [ -z "${RUNNER_TEMP:-}" ]; then
  echo "prepare-macos-performance-binary: RUNNER_TEMP is required" >&2
  exit 2
fi
case $RUNNER_TEMP in
  /*) ;;
  *)
    echo "prepare-macos-performance-binary: RUNNER_TEMP must be absolute" >&2
    exit 2
    ;;
esac

build_root="$RUNNER_TEMP/rottweiler-macos-performance-build.noindex"
artifact_root="$RUNNER_TEMP/rottweiler-macos-performance-artifact.noindex"
rm -rf "$build_root" "$artifact_root"
mkdir -p "$build_root" "$artifact_root"

CARGO_TARGET_DIR="$build_root" scripts/cargo-release.sh build --locked --release -p rw-cli
release_dir=$(CARGO_TARGET_DIR="$build_root" scripts/cargo-release.sh artifact-dir)
install -m 700 "$release_dir/rw" "$artifact_root/rw"
(cd "$artifact_root" && shasum -a 256 rw > rw.sha256)
