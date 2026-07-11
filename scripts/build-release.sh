#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo"

export CARGO_PROFILE_RELEASE_DEBUG=0
cargo build --locked --release -p rw-cli
(cd packages/tui && bun run build)

engine="$repo/target/release/rw"
tui="$repo/packages/tui/dist/rottweiler-tui"
case "$(uname -s)" in
  Darwin) opentui_native_name=libopentui.dylib ;;
  Linux) opentui_native_name=libopentui.so ;;
  MINGW*|MSYS*|CYGWIN*) opentui_native_name=opentui.dll ;;
  *) echo "unsupported release platform: $(uname -s)" >&2; exit 1 ;;
esac
opentui_native="$repo/packages/tui/dist/$opentui_native_name"
engine_bytes=$(wc -c <"$engine" | tr -d ' ')
tui_bytes=$(wc -c <"$tui" | tr -d ' ')
opentui_native_bytes=$(wc -c <"$opentui_native" | tr -d ' ')
tui_bundle_bytes=$((tui_bytes + opentui_native_bytes))
if [ "$engine_bytes" -ge 25000000 ]; then
  echo "release engine is ${engine_bytes} bytes; budget is <25000000" >&2
  exit 1
fi
if [ "$tui_bundle_bytes" -ge 100000000 ]; then
  echo "release TUI bundle is ${tui_bundle_bytes} bytes; budget is <100000000" >&2
  exit 1
fi

version=$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"name":"rw-cli","version":"\([^"]*\)".*/\1/p')
if [ -z "$version" ]; then
  echo "could not determine rw-cli version" >&2
  exit 1
fi
platform=$(uname -s | tr '[:upper:]' '[:lower:]')-$(uname -m)
stage="$repo/dist/rottweiler-$version-$platform"
archive="$stage.tar.gz"
rm -rf "$stage" "$archive"
mkdir -p "$stage/bin"
cp "$engine" "$stage/bin/rw"
cp "$tui" "$stage/bin/rottweiler-tui"
cp "$opentui_native" "$stage/bin/$opentui_native_name"
chmod 755 "$stage/bin/rw" "$stage/bin/rottweiler-tui"
tar -czf "$archive" -C "$repo/dist" "$(basename "$stage")"

verify=$(mktemp -d "${TMPDIR:-/tmp}/rottweiler-release.XXXXXX")
trap 'rm -rf "$verify"' EXIT HUP INT TERM
tar -xzf "$archive" -C "$verify"
installed="$verify/$(basename "$stage")/bin"
test -x "$installed/rw"
test -x "$installed/rottweiler-tui"
test -f "$installed/$opentui_native_name"
"$installed/rw" --version >/dev/null
rm -rf "$verify"
trap - EXIT HUP INT TERM
printf '%s\n' "$archive"
