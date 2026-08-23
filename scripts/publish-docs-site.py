#!/usr/bin/env python3
"""Overlay the docs-owned GitHub Pages tree without touching updates/**."""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import subprocess
from pathlib import Path, PurePosixPath

MANIFEST = ".rottweiler-docs-manifest.json"
RESERVED = {".git", "updates"}


def fail(message: str) -> SystemExit:
    return SystemExit(f"docs publisher: {message}")


def validate_relative(value: object) -> PurePosixPath:
    if not isinstance(value, str) or not value:
        raise fail("manifest paths must be non-empty strings")
    path = PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts or path.parts[0] in RESERVED:
        raise fail(f"unsafe docs-owned path: {value}")
    if path.as_posix() != value or any(part in {"", "."} for part in path.parts):
        raise fail(f"non-canonical docs-owned path: {value}")
    return path


def read_manifest(path: Path) -> list[PurePosixPath]:
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise fail(f"cannot read {path}: {error}") from error
    if not isinstance(document, dict) or document.get("schema_version") != 1:
        raise fail(f"unsupported manifest in {path}")
    files = document.get("files")
    if not isinstance(files, list) or files != sorted(set(files)):
        raise fail(f"manifest paths must be sorted and unique in {path}")
    return [validate_relative(value) for value in files]


def regular_files(root: Path, *, exclude_manifest: bool = False) -> list[PurePosixPath]:
    files: list[PurePosixPath] = []
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root)
        if relative.parts and relative.parts[0] in RESERVED:
            continue
        if path.is_symlink():
            raise fail(f"symlinks are forbidden: {path}")
        if path.is_file():
            value = PurePosixPath(relative.as_posix())
            if exclude_manifest and value.as_posix() == MANIFEST:
                continue
            files.append(validate_relative(value.as_posix()))
    return sorted(files, key=lambda value: value.as_posix())


def digest_tree(root: Path) -> str:
    digest = hashlib.sha256()
    if not root.is_dir() or root.is_symlink():
        raise fail("the Pages checkout must contain a regular updates directory")
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise fail(f"updates must not contain symlinks: {path}")
        if path.is_file():
            relative = path.relative_to(root).as_posix().encode()
            digest.update(len(relative).to_bytes(8, "big"))
            digest.update(relative)
            with path.open("rb") as source:
                while chunk := source.read(1024 * 1024):
                    digest.update(chunk)
    return digest.hexdigest()


def git(checkout: Path, *arguments: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(checkout), *arguments],
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode != 0:
        raise fail(result.stderr.strip() or f"git {' '.join(arguments)} failed")
    return result.stdout.strip()


def remove_empty_parents(path: Path, root: Path) -> None:
    parent = path.parent
    while parent != root and parent not in {root / name for name in RESERVED}:
        try:
            parent.rmdir()
        except OSError:
            break
        parent = parent.parent


def overlay(site: Path, checkout: Path) -> None:
    site = site.resolve(strict=True)
    checkout = checkout.resolve(strict=True)
    if not (checkout / ".git").exists():
        raise fail("checkout is not a Git worktree")

    before_digest = digest_tree(checkout / "updates")
    before_tree = git(checkout, "rev-parse", "HEAD:updates")

    next_manifest = read_manifest(site / MANIFEST)
    observed_site = regular_files(site, exclude_manifest=True)
    if next_manifest != observed_site:
        raise fail("build manifest does not exactly describe the site output")

    prior_manifest_path = checkout / MANIFEST
    if prior_manifest_path.exists():
        prior_files = read_manifest(prior_manifest_path)
    else:
        prior_files = regular_files(checkout, exclude_manifest=True)

    for relative in [*prior_files, PurePosixPath(MANIFEST)]:
        target = checkout.joinpath(*relative.parts)
        if target.is_symlink():
            raise fail(f"refusing to remove symlink from Pages checkout: {relative}")
        if target.is_file():
            target.unlink()
            remove_empty_parents(target, checkout)
        elif target.exists():
            raise fail(f"docs-owned manifest entry is not a regular file: {relative}")

    for relative in [*next_manifest, PurePosixPath(MANIFEST)]:
        source = site.joinpath(*relative.parts)
        target = checkout.joinpath(*relative.parts)
        if source.is_symlink() or not source.is_file():
            raise fail(f"site output is not a regular file: {relative}")
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target, follow_symlinks=False)

    if digest_tree(checkout / "updates") != before_digest:
        raise fail("documentation overlay changed updates content")
    if git(checkout, "rev-parse", "HEAD:updates") != before_tree:
        raise fail("documentation overlay changed the committed updates tree")
    result = subprocess.run(
        ["git", "-C", str(checkout), "diff", "--quiet", "--", "updates"],
        check=False,
    )
    if result.returncode != 0:
        raise fail("documentation overlay produced an updates diff")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--site", required=True, type=Path)
    parser.add_argument("--checkout", required=True, type=Path)
    arguments = parser.parse_args()
    overlay(arguments.site, arguments.checkout)


if __name__ == "__main__":
    main()
