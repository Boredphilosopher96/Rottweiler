#!/bin/sh
set -eu

version='@ROTTWEILER_VERSION@'
packaged_platform='@ROTTWEILER_PLATFORM@'
packaged_root='@ROTTWEILER_RELEASE_ROOT@'
archive_files='@ROTTWEILER_ARCHIVE_FILES@'
archive_directories='@ROTTWEILER_ARCHIVE_DIRECTORIES@'
executable_files='@ROTTWEILER_EXECUTABLE_FILES@'
readonly_files='@ROTTWEILER_READONLY_FILES@'
archive_entry_count='@ROTTWEILER_ARCHIVE_ENTRY_COUNT@'
engine_path='@ROTTWEILER_ENGINE_PATH@'

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
[ "$(basename -- "$root")" = "$packaged_root" ] ||
  fail 'archive directory name does not match the packaged release'

for required in $archive_files; do
  [ -f "$root/$required" ] && [ ! -L "$root/$required" ] ||
    fail "archive is missing regular file $required"
done
for executable in $executable_files; do
  [ -x "$root/$executable" ] || fail "$executable is not executable"
done
entry_count=$(find "$root" -mindepth 1 -print | wc -l | tr -d ' ')
[ "$entry_count" = "$archive_entry_count" ] || fail 'archive contains unexpected entries'
[ -z "$(find "$root" ! -type f ! -type d -print -quit)" ] ||
  fail 'archive contains a link or special filesystem object'
[ -z "$(find "$root" -type f -links +1 -print -quit)" ] ||
  fail 'archive contains a hard-linked file'

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
lock_pid="$lock/pid"
stat_mode() {
  case "$(uname -s)" in
    Darwin) stat -f '%Lp' -- "$1" ;;
    *) stat -c '%a' -- "$1" ;;
  esac
}
stat_uid() {
  case "$(uname -s)" in
    Darwin) stat -f '%u' -- "$1" ;;
    *) stat -c '%u' -- "$1" ;;
  esac
}
reclaim_stale_lock() {
  [ -d "$lock" ] && [ ! -L "$lock" ] || return 1
  [ "$(stat_mode "$lock" 2>/dev/null)" = 700 ] || return 1
  [ "$(stat_uid "$lock" 2>/dev/null)" = "$(id -u)" ] || return 1
  [ "$(find "$lock" -mindepth 1 -maxdepth 1 -print | wc -l | tr -d ' ')" = 1 ] || return 1
  [ -f "$lock_pid" ] && [ ! -L "$lock_pid" ] || return 1
  [ "$(stat_mode "$lock_pid" 2>/dev/null)" = 600 ] || return 1
  [ "$(stat_uid "$lock_pid" 2>/dev/null)" = "$(id -u)" ] || return 1
  [ "$(wc -c < "$lock_pid" | tr -d ' ')" -le 64 ] || return 1
  owner_pid=$(tr -d '\n' < "$lock_pid")
  case "$owner_pid" in
    ''|*[!0-9]*) return 1 ;;
  esac
  [ "$owner_pid" -gt 0 ] || return 1
  kill -0 "$owner_pid" 2>/dev/null && return 1
  rm -f "$lock_pid" || return 1
  rmdir "$lock" || return 1
}
if ! mkdir "$lock" 2>/dev/null; then
  reclaim_stale_lock || fail 'another install or upgrade is already running'
  mkdir "$lock" 2>/dev/null || fail 'another install or upgrade is already running'
fi
chmod 700 "$lock"
if ! (umask 077 && printf '%s\n' "$$" > "$lock_pid"); then
  rmdir "$lock" 2>/dev/null || true
  fail 'could not record install lock ownership'
fi
staging=
temporary_current=
temporary_bin=
cleanup() {
  [ -z "$staging" ] || rm -rf "$staging"
  [ -z "$temporary_current" ] || rm -f "$temporary_current"
  [ -z "$temporary_bin" ] || rm -f "$temporary_bin"
  rm -f "$lock_pid"
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
  [ "$(find "$version_dir" -mindepth 1 -print | wc -l | tr -d ' ')" = "$archive_entry_count" ] ||
    fail 'existing version generation contains unexpected entries'
  [ -z "$(find "$version_dir" ! -type f ! -type d -print -quit)" ] ||
    fail 'existing version generation contains a link or special filesystem object'
  [ -z "$(find "$version_dir" -type f -links +1 -print -quit)" ] ||
    fail 'existing version generation contains a hard-linked file'
  for relative in $archive_files; do
    [ -f "$version_dir/$relative" ] && [ ! -L "$version_dir/$relative" ] ||
      fail 'existing version generation is incomplete'
    cmp -s "$root/$relative" "$version_dir/$relative" ||
      fail 'existing version generation differs from this signed release'
  done
else
  staging=$(mktemp -d "$prefix/.staging-$version.XXXXXX") ||
    fail 'could not create same-filesystem staging directory'
  for directory in $archive_directories; do
    mkdir "$staging/$directory"
    chmod 755 "$staging/$directory"
  done
  for relative in $archive_files; do
    cp "$root/$relative" "$staging/$relative"
  done
  for executable in $executable_files; do
    chmod 755 "$staging/$executable"
  done
  for readonly in $readonly_files; do
    chmod 644 "$staging/$readonly"
  done
  version_output=$("$staging/$engine_path" --version) || fail 'staged rw failed its version check'
  [ "$version_output" = "rw $version" ] || fail 'staged rw reported the wrong version'
  "$staging/$engine_path" __install-sync \
@ROTTWEILER_STAGING_SYNC_ARGUMENTS@
    "$staging" || fail 'staged generation could not be flushed durably'
  mv "$staging" "$version_dir"
  staging=
fi

"$version_dir/$engine_path" __install-sync \
@ROTTWEILER_VERSION_SYNC_ARGUMENTS@
  "$version_dir" \
  "$prefix/versions" \
  "$prefix" || fail 'installed generation could not be flushed durably'

if [ -e "$prefix/current" ] && [ ! -L "$prefix/current" ]; then
  fail 'current selector is not a symlink'
fi
temporary_current="$prefix/.current.$$"
ln -s "versions/$version" "$temporary_current"
mv -f "$temporary_current" "$prefix/current"
temporary_current=
"$version_dir/bin/rw" __install-sync "$prefix" ||
  fail 'current selector could not be flushed durably'

if [ -e "$prefix/bin/rw" ] && [ ! -L "$prefix/bin/rw" ]; then
  fail 'bin/rw is not a managed symlink'
fi
temporary_bin="$prefix/bin/.rw.$$"
ln -s "../current/$engine_path" "$temporary_bin"
mv -f "$temporary_bin" "$prefix/bin/rw"
temporary_bin=
"$version_dir/$engine_path" __install-sync "$prefix/bin" "$prefix" ||
  fail 'managed launcher could not be flushed durably'

cleanup
trap - EXIT HUP INT TERM
printf 'installed Rottweiler %s at %s\n' "$version" "$prefix"
printf 'add %s/bin to PATH if needed\n' "$prefix"
