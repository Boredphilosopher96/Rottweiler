"""Bind every file in a functional-test bundle to its candidate and platform."""
import argparse
import hashlib
import json
from pathlib import Path
import re

MANIFEST = "bundle-identity.json"
MAX_MANIFEST_BYTES = 4 * 1024 * 1024
MAX_FILES = 10_000


def contents(root: Path) -> dict:
    files = {}
    if root.is_symlink() or not root.is_dir():
        raise ValueError("bundle root must be a real directory")
    for path in root.rglob("*"):
        if path.is_symlink():
            raise ValueError("bundle cannot contain symlinks")
        if path.is_dir() or path == root / MANIFEST:
            continue
        if not path.is_file():
            raise ValueError("bundle contains a non-regular entry")
        digest = hashlib.sha256()
        count = 0
        with path.open("rb") as stream:
            while block := stream.read(1024 * 1024):
                digest.update(block)
                count += len(block)
        files[path.relative_to(root).as_posix()] = {"sha256": digest.hexdigest(), "bytes": count}
        if len(files) > MAX_FILES:
            raise ValueError("bundle exceeds file-count bound")
    if not files:
        raise ValueError("empty bundle")
    return files


def document(root: Path, source_sha: str, platform: str) -> dict:
    if re.fullmatch(r"[0-9a-f]{40}", source_sha) is None:
        raise ValueError("invalid source SHA")
    return {"schema_version": 1, "source_sha": source_sha, "platform": platform, "files": contents(root)}


def verify(root: Path, source_sha: str, platform: str) -> None:
    manifest = root / MANIFEST
    if root.is_symlink() or manifest.is_symlink() or not manifest.is_file():
        raise ValueError("bundle identity must be a regular file")
    with manifest.open("rb") as stream:
        raw = stream.read(MAX_MANIFEST_BYTES + 1)
    if len(raw) > MAX_MANIFEST_BYTES:
        raise ValueError("bundle identity exceeds byte bound")
    if json.loads(raw) != document(root, source_sha, platform):
        raise ValueError("bundle identity or contents differ from the candidate")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["create", "verify"])
    parser.add_argument("root", type=Path)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--platform", required=True)
    args = parser.parse_args()
    if args.command == "create":
        (args.root / MANIFEST).write_text(json.dumps(document(args.root, args.source_sha, args.platform), sort_keys=True) + "\n")
    else:
        verify(args.root, args.source_sha, args.platform)


if __name__ == "__main__":
    main()
