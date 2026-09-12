#!/usr/bin/env bash
set -euo pipefail

export PATH="$HOME/.cargo/bin:$HOME/.bun/bin:$PATH"

run_doctor_acceptance() {
  local rw=$1
  local doctor_home=$2
  local doctor_report=$3
  local doctor_root="$doctor_home/.rottweiler"
  local doctor_status=0

  mkdir -p "$doctor_root"
  chmod 700 "$doctor_home" "$doctor_root"
  cat > "$doctor_root/config.toml" <<'TOML'
[models]
default = "offline"
aliases.offline = ["fixture/offline"]

[providers.fixture]
kind = "openai_chat"
base_url = "http://127.0.0.1:9/v1/chat/completions"
TOML
  chmod 600 "$doctor_root/config.toml"

  HOME="$doctor_home" ROTTWEILER_HOME="$doctor_root" TERM=xterm-256color \
    "$rw" doctor --json > "$doctor_report" || doctor_status=$?
  if [[ "$doctor_status" -ne 0 ]]; then
    cat "$doctor_report" >&2
    return "$doctor_status"
  fi

  if ! python3 - "$doctor_report" <<'PY'
import json
import sys
from pathlib import Path

report = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
checks = {item["id"]: item for item in report["checks"]}
if report.get("healthy") is not True:
    raise SystemExit("WSL doctor report is unhealthy")
if report.get("network_probes_requested") is not False:
    raise SystemExit("WSL doctor unexpectedly requested network probes")
if checks.get("providers", {}).get("code") != "provider_configured":
    raise SystemExit("doctor did not load the offline provider fixture")
if checks.get("models.default", {}).get("code") != "default_model_resolved":
    raise SystemExit("doctor did not resolve the offline default model")
if checks.get("provider.fixture.auth", {}).get("code") != "credential_not_required":
    raise SystemExit("offline provider fixture unexpectedly requires credentials")
if checks.get("provider.fixture.reachability", {}).get("code") != "network_probe_not_requested":
    raise SystemExit("offline provider fixture unexpectedly performed a network probe")
if checks.get("os", {}).get("details", {}).get("wsl") != "true":
    raise SystemExit("doctor did not identify the real WSL environment")
if checks.get("sandbox", {}).get("status") != "pass":
    raise SystemExit("WSL sandbox support is unavailable")
PY
  then
    cat "$doctor_report" >&2
    return 1
  fi
}

main() {
if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "usage: $0 WSL_PATH_TO_WINDOWS_CHECKOUT [WSL_PATH_TO_RELEASE_ARCHIVE]" >&2
  exit 2
fi

source_root=${1%/}
[[ -d "$source_root" && "$source_root" == /mnt/* ]] || {
  echo "checkout must be supplied through a real WSL mount" >&2
  exit 1
}
command -v cargo >/dev/null
command -v rustup >/dev/null

work=$(mktemp -d "$HOME/rottweiler-wsl-acceptance.XXXXXX")
drvfs_prefix="/mnt/c/rottweiler-wsl-refusal-$$"
trap 'rm -rf "$work" "$drvfs_prefix"' EXIT
mkdir -p "$work/repo"
cp -a "$source_root/." "$work/repo/"
cd "$work/repo"

rustup toolchain install 1.97.1 --profile minimal --component clippy,rustfmt
rustup override set 1.97.1

export ROTTWEILER_CREDENTIAL_BACKEND=file
if [[ $# -eq 2 ]]; then
  source_archive=$2
  case "$source_archive" in
    "$source_root"/dist/wsl-release/rottweiler-*-linux-x86_64.tar.gz) ;;
    *)
      echo "release archive must be the downloaded linux-x86_64 workflow artifact" >&2
      exit 1
      ;;
  esac
  [[ -f "$source_archive" && ! -L "$source_archive" ]] || {
    echo "release archive must be a regular non-symlink file" >&2
    exit 1
  }
  command -v sha256sum >/dev/null
  archive="$work/$(basename -- "$source_archive")"
  expected_digest=$(sha256sum -- "$source_archive" | awk '{print $1}')
  cp -- "$source_archive" "$archive"
  actual_digest=$(sha256sum -- "$archive" | awk '{print $1}')
  [[ "$actual_digest" == "$expected_digest" ]] || {
    echo "release archive changed while it was copied to the Linux filesystem" >&2
    exit 1
  }
else
  command -v bun >/dev/null
  (cd packages/tui && bun install --frozen-lockfile)
  archive=$(python3 scripts/build-native-candidate.py --print archive)
fi
mkdir "$work/release"
tar -xzf "$archive" -C "$work/release"
release=$(find "$work/release" -mindepth 1 -maxdepth 1 -type d -print)
[[ -n "$release" && $(printf '%s\n' "$release" | wc -l) -eq 1 ]]

prefix="$work/installed"
"$release/install.sh" --prefix "$prefix"
"$prefix/bin/rw" --version

run_doctor_acceptance "$prefix/bin/rw" "$work/home" "$work/doctor.json"

if "$release/install.sh" --prefix "$drvfs_prefix" >"$work/drvfs.out" 2>"$work/drvfs.err"; then
  echo "installer unexpectedly accepted a DrvFS prefix" >&2
  exit 1
fi
grep -Eiq 'DrvFS|Windows-mounted|WSL.*mount' "$work/drvfs.err"

ROTTWEILER_REQUIRE_LINUX_SANDBOX=1 cargo test --locked -p rw-sandbox --test linux_egress
ROTTWEILER_REQUIRE_LINUX_SANDBOX=1 cargo test --locked -p rw-sandbox --test linux_helper_driver
ROTTWEILER_REQUIRE_LINUX_SANDBOX=1 cargo test --locked -p rw-tools --test linux_safe_list_network
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
