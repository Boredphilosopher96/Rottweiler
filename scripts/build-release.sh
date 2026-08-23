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
(cd packages/plugin-sdk && bun install --frozen-lockfile && bun run typecheck && bun test && bun run build)
(cd packages/plugin-host && bun install --frozen-lockfile && bun run typecheck && bun test && bun run build)

release_dir=$(scripts/cargo-release.sh artifact-dir)
engine="$release_dir/rw"
wasm_host="$release_dir/rottweiler-wasm-host"
plugin_host="$repo/packages/plugin-host/dist/rottweiler-plugin-host"
tui="$repo/packages/tui/dist/rottweiler-tui"
platform=$(python3 scripts/release_contract.py resolve-platform --system "$(uname -s)" --machine "$(uname -m)")
opentui_native_name=$(python3 scripts/release_contract.py platform-field --platform "$platform" --field native-library)
opentui_native="$repo/packages/tui/dist/$opentui_native_name"
python3 scripts/release_contract.py validate-build \
  --platform "$platform" \
  --engine "$engine" \
  --wasm-host "$wasm_host" \
  --plugin-host "$plugin_host" \
  --tui "$tui" \
  --opentui-native "$opentui_native"

version=$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"name":"rw-cli","version":"\([^"]*\)".*/\1/p')
if [ -z "$version" ]; then
  echo "could not determine rw-cli version" >&2
  exit 1
fi
release_root=$(python3 scripts/release_contract.py archive-root --version "$version" --platform "$platform")
stage="$repo/dist/$release_root"
archive="$stage.tar.gz"
rm -rf "$stage" "$archive"
python3 scripts/release_contract.py stage-release \
  --output "$stage" \
  --template scripts/install-release.sh \
  --version "$version" \
  --platform "$platform" \
  --engine "$engine" \
  --wasm-host "$wasm_host" \
  --plugin-host "$plugin_host" \
  --tui "$tui" \
  --opentui-native "$opentui_native"
python3 scripts/package-release.py "$stage" "$archive"
python3 scripts/release_contract.py verify-archive \
  --archive "$archive" \
  --version "$version" \
  --platform "$platform"

verify=$(mktemp -d "${TMPDIR:-/tmp}/rottweiler-release.XXXXXX")
trap 'rm -rf "$verify"' EXIT HUP INT TERM
tar -xzf "$archive" -C "$verify"
engine_path=$(python3 scripts/release_contract.py member-path --platform "$platform" --member engine)
plugin_host_path=$(python3 scripts/release_contract.py member-path --platform "$platform" --member plugin_host)
"$verify/$release_root/$engine_path" --version >/dev/null
"$verify/$release_root/$plugin_host_path" version | python3 -c '
import json, sys
identity = json.load(sys.stdin)
if set(identity) != {"abi", "format"} or not isinstance(identity["abi"], int) or identity["abi"] < 1 or not isinstance(identity["format"], str) or not identity["format"]:
    raise SystemExit("extracted plugin host reported an unexpected semantic identity")
'
rm -rf "$verify"
trap - EXIT HUP INT TERM
printf '%s\n' "$archive"
