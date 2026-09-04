#!/usr/bin/env bash
set -euo pipefail
# Reviewed Linux CI tool archive; local installations can call actionlint directly.
version=1.7.12
sha256=8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
curl --fail --silent --show-error --location --max-time 60 \
  "https://github.com/rhysd/actionlint/releases/download/v${version}/actionlint_${version}_linux_amd64.tar.gz" \
  --output "$work/actionlint.tar.gz"
printf '%s  %s\n' "$sha256" "$work/actionlint.tar.gz" | sha256sum -c -
tar -xzf "$work/actionlint.tar.gz" -C "$work" actionlint
cd "$root"
"$work/actionlint" -shellcheck='' -pyflakes=''
