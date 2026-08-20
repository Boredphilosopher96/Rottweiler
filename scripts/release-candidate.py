#!/usr/bin/env python3
"""Create or verify exact-SHA release preflight evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
from typing import Any


REPOSITORY_PATTERN = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")
SHA_PATTERN = re.compile(r"[0-9a-f]{40}")
VERSION_PATTERN = re.compile(
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
    r"(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?"
)


def positive_integer(value: str) -> int:
    parsed = int(value)
    if parsed < 1:
        raise argparse.ArgumentTypeError("value must be a positive integer")
    return parsed


def digest_tree(path: Path) -> str:
    root = path.resolve(strict=True)
    files = [root] if root.is_file() else sorted(root.rglob("*"))
    regular_files = [candidate for candidate in files if candidate.is_file()]
    if not regular_files:
        raise ValueError(f"release evidence is empty: {path}")
    digest = hashlib.sha256()
    for candidate in regular_files:
        if candidate.is_symlink():
            raise ValueError(f"release evidence cannot contain symlinks: {candidate}")
        relative = candidate.name if root.is_file() else candidate.relative_to(root).as_posix()
        payload = candidate.read_bytes()
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        digest.update(str(len(payload)).encode("ascii"))
        digest.update(b"\0")
        digest.update(payload)
    return digest.hexdigest()


def expected_document(arguments: argparse.Namespace) -> dict[str, Any]:
    if REPOSITORY_PATTERN.fullmatch(arguments.repository) is None:
        raise ValueError("repository must be an OWNER/NAME pair")
    if SHA_PATTERN.fullmatch(arguments.source_sha) is None:
        raise ValueError("source SHA must be 40 lowercase hexadecimal characters")
    if VERSION_PATTERN.fullmatch(arguments.version) is None:
        raise ValueError("version must be canonical semantic version without a leading v")
    major = int(arguments.version.split(".", 1)[0])
    qualification = "pre-v1" if major == 0 else "v1"
    artifact_suffix = f"{arguments.run_id}-{arguments.run_attempt}"
    return {
        "schema_version": 1,
        "repository": arguments.repository,
        "source_sha": arguments.source_sha,
        "version": arguments.version,
        "tag": f"v{arguments.version}",
        "release_major": major,
        "qualification": qualification,
        "preflight": {
            "workflow": ".github/workflows/release-preflight.yml",
            "head_branch": "main",
            "run_id": arguments.run_id,
            "run_attempt": arguments.run_attempt,
        },
        "evidence": {
            "repository_prerequisites": "passed",
            "protected_performance": "passed",
        },
        "artifacts": {
            "readiness": {
                "name": f"release-preflight-{artifact_suffix}",
                "sha256": digest_tree(arguments.readiness),
            },
            "linux_performance": {
                "name": f"manual-performance-linux-x86_64-{artifact_suffix}",
                "sha256": digest_tree(arguments.linux_evidence),
            },
            "darwin_performance": {
                "name": f"manual-performance-darwin-arm64-{artifact_suffix}",
                "sha256": digest_tree(arguments.darwin_evidence),
            },
        },
    }


def write_atomic(path: Path, document: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(
        json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    temporary.replace(path)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=("create", "verify"))
    parser.add_argument("--path", required=True, type=Path)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--run-id", required=True, type=positive_integer)
    parser.add_argument("--run-attempt", required=True, type=positive_integer)
    parser.add_argument("--readiness", required=True, type=Path)
    parser.add_argument("--linux-evidence", required=True, type=Path)
    parser.add_argument("--darwin-evidence", required=True, type=Path)
    arguments = parser.parse_args()

    expected = expected_document(arguments)
    if arguments.command == "create":
        write_atomic(arguments.path, expected)
        return 0

    try:
        observed = json.loads(arguments.path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError(f"could not read release candidate evidence: {error}") from error
    if observed != expected:
        raise ValueError("release candidate evidence does not match the exact preflight run")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
