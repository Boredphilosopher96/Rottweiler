#!/bin/sh
set -eu

version='@ROTTWEILER_VERSION@'
packaged_platform='@ROTTWEILER_PLATFORM@'

fail() {
  printf 'rottweiler install: %s\n' "$*" >&2
  exit 1
}

case "$(uname -s)" in
  Darwin) actual_platform="darwin-$(uname -m)" ;;
  Linux) actual_platform="linux-$(uname -m)" ;;
  *) fail "unsupported platform: $(uname -s)" ;;
esac
[ "$actual_platform" = "$packaged_platform" ] ||
  fail "archive is for $packaged_platform, not $actual_platform"

prefix=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --prefix)
      [ "$#" -ge 2 ] || fail "--prefix requires an absolute path"
      prefix=$2
      shift 2
      ;;
    --help|-h)
      printf 'usage: ./install.sh [--prefix ABSOLUTE_PATH]\n'
      exit 0
      ;;
    *) fail "unknown argument: $1" ;;
  esac
done
if [ -z "$prefix" ]; then
  [ -n "${HOME:-}" ] || fail 'HOME is unavailable; pass --prefix'
  prefix="$HOME/.local/share/rottweiler"
fi
prefix=${prefix%/}
case "$prefix" in
  /*) ;;
  *) fail '--prefix must be absolute' ;;
esac
[ "$prefix" != / ] || fail 'refusing to use the filesystem root as the prefix'

root=$(CDPATH= cd -P -- "$(dirname -- "$0")" && pwd)
[ "$(basename -- "$root")" = "rottweiler-$version-$packaged_platform" ] ||
  fail 'archive directory name does not match the packaged release'

native=libopentui.so
[ "$(uname -s)" = Darwin ] && native=libopentui.dylib
for required in install.sh bin/rw bin/rottweiler-tui "bin/$native"; do
  [ -f "$root/$required" ] && [ ! -L "$root/$required" ] ||
    fail "archive is missing regular file $required"
done
[ -x "$root/bin/rw" ] || fail 'bin/rw is not executable'
[ -x "$root/bin/rottweiler-tui" ] || fail 'bin/rottweiler-tui is not executable'
entry_count=$(find "$root" -mindepth 1 -print | wc -l | tr -d ' ')
[ "$entry_count" = 5 ] || fail 'archive contains unexpected entries'
[ -z "$(find "$root" ! -type f ! -type d -print -quit)" ] ||
  fail 'archive contains a link or special filesystem object'

if [ -L "$prefix" ]; then
  fail 'installation prefix must not be a symlink'
fi
mkdir -p "$prefix"
[ -d "$prefix" ] || fail 'installation prefix is not a directory'
chmod 700 "$prefix"

if [ "$(uname -s)" = Linux ]; then
  case "$(uname -r)" in
    *[Mm][Ii][Cc][Rr][Oo][Ss][Oo][Ff][Tt]*)
      filesystem=$(df -T -P "$prefix" 2>/dev/null | awk 'NR == 2 { print $2 }')
      case "$filesystem" in
        9p|drvfs) fail 'WSL installs on DrvFS are unsupported; use the Linux filesystem' ;;
      esac
      ;;
  esac
fi

lock="$prefix/.install-lock"
mkdir "$lock" 2>/dev/null || fail 'another install or upgrade is already running'
staging=
temporary_current=
temporary_bin=
cleanup() {
  [ -z "$staging" ] || rm -rf "$staging"
  [ -z "$temporary_current" ] || rm -f "$temporary_current"
  [ -z "$temporary_bin" ] || rm -f "$temporary_bin"
  rmdir "$lock" 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM

for directory in versions bin; do
  candidate="$prefix/$directory"
  [ ! -L "$candidate" ] || fail "$candidate must not be a symlink"
  mkdir -p "$candidate"
  [ -d "$candidate" ] || fail "$candidate is not a directory"
done
version_dir="$prefix/versions/$version"
if [ -e "$version_dir" ] || [ -L "$version_dir" ]; then
  [ -d "$version_dir" ] && [ ! -L "$version_dir" ] ||
    fail 'existing version generation is unsafe'
  for relative in bin/rw bin/rottweiler-tui "bin/$native"; do
    [ -f "$version_dir/$relative" ] && [ ! -L "$version_dir/$relative" ] ||
      fail 'existing version generation is incomplete'
    cmp -s "$root/$relative" "$version_dir/$relative" ||
      fail 'existing version generation differs from this signed release'
  done
else
  staging=$(mktemp -d "$prefix/.staging-$version.XXXXXX") ||
    fail 'could not create same-filesystem staging directory'
  mkdir "$staging/bin"
  cp "$root/bin/rw" "$staging/bin/rw"
  cp "$root/bin/rottweiler-tui" "$staging/bin/rottweiler-tui"
  cp "$root/bin/$native" "$staging/bin/$native"
  chmod 755 "$staging/bin/rw" "$staging/bin/rottweiler-tui"
  chmod 644 "$staging/bin/$native"
  version_output=$("$staging/bin/rw" --version) || fail 'staged rw failed its version check'
  case "$version_output" in
    "rw $version"*) ;;
    *) fail 'staged rw reported the wrong version' ;;
  esac
  mv "$staging" "$version_dir"
  staging=
fi

if [ -e "$prefix/current" ] && [ ! -L "$prefix/current" ]; then
  fail 'current selector is not a symlink'
fi
temporary_current="$prefix/.current.$$"
ln -s "versions/$version" "$temporary_current"
mv -f "$temporary_current" "$prefix/current"
temporary_current=

if [ -e "$prefix/bin/rw" ] && [ ! -L "$prefix/bin/rw" ]; then
  fail 'bin/rw is not a managed symlink'
fi
temporary_bin="$prefix/bin/.rw.$$"
ln -s '../current/bin/rw' "$temporary_bin"
mv -f "$temporary_bin" "$prefix/bin/rw"
temporary_bin=

cleanup
trap - EXIT HUP INT TERM
printf 'installed Rottweiler %s at %s\n' "$version" "$prefix"
printf 'add %s/bin to PATH if needed\n' "$prefix"
