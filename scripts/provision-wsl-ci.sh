#!/usr/bin/env bash
set -euo pipefail

if [[ ${WSL_INTEROP:-} == "" ]]; then
  echo "provision-wsl-ci.sh must run inside WSL" >&2
  exit 2
fi
if [[ $(id -u) -ne 0 ]]; then
  echo "provision-wsl-ci.sh requires the disposable WSL root user" >&2
  exit 2
fi

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
  build-essential \
  ca-certificates \
  curl \
  git \
  libssl-dev \
  pkg-config \
  unzip

export RUSTUP_VERSION=1.28.2
export RUSTUP_INIT_SKIP_PATH_CHECK=yes
curl --proto '=https' --tlsv1.2 --fail --silent --show-error \
  https://sh.rustup.rs \
  | sh -s -- -y --profile minimal --default-toolchain none

export BUN_INSTALL="$HOME/.bun"
curl --proto '=https' --tlsv1.2 --fail --silent --show-error \
  https://bun.sh/install \
  | bash -s -- bun-v1.3.14

"$HOME/.cargo/bin/rustup" --version
"$HOME/.bun/bin/bun" --version
