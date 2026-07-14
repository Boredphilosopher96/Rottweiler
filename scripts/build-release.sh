#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo"

export CARGO_PROFILE_RELEASE_DEBUG=0
if [ -z "${SOURCE_DATE_EPOCH:-}" ]; then
  SOURCE_DATE_EPOCH=$(git show -s --format=%ct HEAD 2>/dev/null || printf '%s' 1700000000)
  export SOURCE_DATE_EPOCH
fi
# Build the public entrypoint in its own Cargo invocation. Cargo unifies
# dependency features across selected roots, so combining these packages would
# accidentally relink Wasmtime into `rw` even though only the helper uses it.
scripts/cargo-release.sh build --locked --release -p rw-cli
scripts/cargo-release.sh build --locked --release -p rw-wasm-host
(cd packages/tui && bun run build)

release_dir=$(scripts/cargo-release.sh artifact-dir)
engine="$release_dir/rw"
wasm_host="$release_dir/rottweiler-wasm-host"
tui="$repo/packages/tui/dist/rottweiler-tui"
case "$(uname -s)" in
  Darwin) opentui_native_name=libopentui.dylib; tui_bundle_limit=100000000 ;;
  Linux) opentui_native_name=libopentui.so; tui_bundle_limit=110000000 ;;
  MINGW*|MSYS*|CYGWIN*) opentui_native_name=opentui.dll; tui_bundle_limit=100000000 ;;
  *) echo "unsupported release platform: $(uname -s)" >&2; exit 1 ;;
esac
opentui_native="$repo/packages/tui/dist/$opentui_native_name"
engine_bytes=$(wc -c <"$engine" | tr -d ' ')
wasm_host_bytes=$(wc -c <"$wasm_host" | tr -d ' ')
tui_bytes=$(wc -c <"$tui" | tr -d ' ')
opentui_native_bytes=$(wc -c <"$opentui_native" | tr -d ' ')
tui_bundle_bytes=$((tui_bytes + opentui_native_bytes))
if [ "$engine_bytes" -ge 25000000 ]; then
  echo "release engine is ${engine_bytes} bytes; budget is <25000000" >&2
  exit 1
fi
if [ "$wasm_host_bytes" -ge 30000000 ]; then
  echo "release WASM helper is ${wasm_host_bytes} bytes; budget is <30000000" >&2
  exit 1
fi
if [ "$tui_bundle_bytes" -ge "$tui_bundle_limit" ]; then
  echo "release TUI bundle is ${tui_bundle_bytes} bytes; budget is <${tui_bundle_limit}" >&2
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
cp "$wasm_host" "$stage/bin/rottweiler-wasm-host"
cp "$tui" "$stage/bin/rottweiler-tui"
cp "$opentui_native" "$stage/bin/$opentui_native_name"
chmod 755 "$stage/bin/rw" "$stage/bin/rottweiler-tui" "$stage/bin/rottweiler-wasm-host"
sed \
  -e "s/@ROTTWEILER_VERSION@/$version/g" \
  -e "s/@ROTTWEILER_PLATFORM@/$platform/g" \
  scripts/install-release.sh >"$stage/install.sh"
chmod 755 "$stage/install.sh"
python3 scripts/package-release.py "$stage" "$archive"

verify=$(mktemp -d "${TMPDIR:-/tmp}/rottweiler-release.XXXXXX")
trap 'rm -rf "$verify"' EXIT HUP INT TERM
tar -xzf "$archive" -C "$verify"
installed="$verify/$(basename "$stage")/bin"
test -x "$installed/rw"
test -x "$installed/rottweiler-tui"
test -x "$installed/rottweiler-wasm-host"
test -f "$installed/$opentui_native_name"
"$installed/rw" --version >/dev/null
rm -rf "$verify"
trap - EXIT HUP INT TERM
printf '%s\n' "$archive"
