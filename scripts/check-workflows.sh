#!/usr/bin/env bash
set -euo pipefail
# Pinned host-native archives; CI and maintainer checks run the same linter.
version=1.7.12
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) platform=linux_amd64; sha256=8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8 ;;
  Linux-aarch64) platform=linux_arm64; sha256=325e971b6ba9bfa504672e29be93c24981eeb1c07576d730e9f7c8805afff0c6 ;;
  Darwin-x86_64) platform=darwin_amd64; sha256=5b44c3bc2255115c9b69e30efc0fecdf498fdb63c5d58e17084fd5f16324c644 ;;
  Darwin-arm64) platform=darwin_arm64; sha256=aba9ced2dee8d27fecca3dc7feb1a7f9a52caefa1eb46f3271ea66b6e0e6953f ;;
  *) echo "workflow lint requires a supported Linux or macOS host" >&2; exit 1 ;;
esac
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
curl --fail --silent --show-error --location --max-time 60 \
  "https://github.com/rhysd/actionlint/releases/download/v${version}/actionlint_${version}_${platform}.tar.gz" \
  --output "$work/actionlint.tar.gz"
python3 - "$work/actionlint.tar.gz" "$sha256" <<'PYTHON'
import hashlib
import sys
with open(sys.argv[1], "rb") as archive:
    digest = hashlib.sha256()
    for chunk in iter(lambda: archive.read(65536), b""):
        digest.update(chunk)
if digest.hexdigest() != sys.argv[2]:
    raise SystemExit("actionlint archive checksum mismatch")
PYTHON
tar -xzf "$work/actionlint.tar.gz" -C "$work" actionlint
cd "$root"
"$work/actionlint" -shellcheck='' -pyflakes=''
