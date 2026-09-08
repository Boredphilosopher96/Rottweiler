"""Exact native candidate and explicitly prepared MCP fixture acceptance inputs."""
from __future__ import annotations

import json
from pathlib import Path
import stat

import native_candidate
import native_profile

FIXTURE = "rw-mcp-fixture"
RECEIPT = "fixture.json"


def fixture_identity(path: Path) -> dict:
    metadata = path.lstat()
    if (not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1
            or stat.S_IMODE(metadata.st_mode) != 0o500
            or not 0 < metadata.st_size <= 256 * 1024 * 1024):
        raise ValueError("M8 fixture must be a bounded private immutable executable")
    return {"name": FIXTURE, "bytes": metadata.st_size, "sha256": native_candidate.hash_file(path)}


def verify(candidate: Path, fixture_receipt: Path, repo: Path) -> dict:
    product = native_candidate.verify(candidate, repo)
    if fixture_receipt.is_symlink() or not fixture_receipt.is_file() or fixture_receipt.stat().st_size > 128 * 1024:
        raise ValueError("M8 fixture receipt must be a bounded regular file")
    fixture = fixture_receipt.parent / FIXTURE
    prepared = json.loads(fixture_receipt.read_text())
    expected = {"schema_version": 1, "candidate_identity": product["identity_sha256"],
                "source": product["identity"]["source"], "target": product["identity"]["target"],
                "rust": product["identity"]["toolchains"]["rust"],
                "profile": native_profile.settings(product["identity"]["target"], repo),
                "fixture": fixture_identity(fixture)}
    if prepared != expected:
        raise ValueError("M8 prepared fixture source/profile/artifact differs from candidate")
    return {"candidate": str(candidate.resolve()), "candidate_receipt": product,
            "fixture_receipt": str(fixture_receipt.resolve()),
            "fixture_receipt_sha256": native_candidate.hash_file(fixture_receipt), "prepared": prepared}
