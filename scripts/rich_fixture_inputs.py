"""Prepared SDK inputs paired with one checksum-verified native candidate."""
from __future__ import annotations
import hashlib
import json
import os
from pathlib import Path
import stat
import native_candidate

RECEIPT = "rich-fixture.json"
FILES = {"plugin.js": 2 * 1024 * 1024, "manifest.json": 64 * 1024}


def file_identity(path: Path, limit: int) -> dict:
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        info = os.fstat(descriptor)
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or not 0 < info.st_size <= limit:
            raise ValueError("rich fixture requires a bounded, unlinked regular file")
        digest, count = hashlib.sha256(), 0
        while block := os.read(descriptor, min(1024 * 1024, limit + 1 - count)):
            count += len(block)
            if count > limit:
                raise ValueError("rich fixture grew beyond its byte bound")
            digest.update(block)
        def identity(value):
            return (value.st_dev, value.st_ino, value.st_mode, value.st_nlink, value.st_size,
                    value.st_mtime_ns, value.st_ctime_ns)
        if count != info.st_size or identity(info) != identity(os.fstat(descriptor)) or identity(info) != identity(path.lstat()):
            raise ValueError("rich fixture changed during identity capture")
        return {"bytes": count, "sha256": digest.hexdigest()}
    finally:
        os.close(descriptor)



def verify(candidate: Path, receipt_path: Path, repo: Path) -> dict:
    product = native_candidate.verify(candidate, repo)
    file_identity(receipt_path, 16 * 1024)
    receipt = json.loads(receipt_path.read_bytes())
    if not isinstance(receipt, dict) or set(receipt) != {"schema_version", "candidate_identity", "source", "bun", "files"}:
        raise ValueError("invalid rich fixture receipt contract")
    if (receipt["schema_version"] != 1 or receipt["candidate_identity"] != product["identity_sha256"]
            or receipt["source"] != product["identity"]["source"]):
        raise ValueError("rich fixture differs from the candidate source")
    if not isinstance(receipt["bun"], dict) or set(receipt["bun"]) != {"path", "version", "bytes", "sha256"}:
        raise ValueError("rich fixture requires exact Bun authority")
    bun = Path(receipt["bun"]["path"])
    if not bun.is_absolute() or bun.resolve(strict=True) != bun or not os.access(bun, os.X_OK):
        raise ValueError("rich fixture Bun path is not canonical")
    if receipt["bun"] != {"path": str(bun), "version": (repo / ".bun-version").read_text().strip(),
                          **file_identity(bun, 256 * 1024 * 1024)}:
        raise ValueError("rich fixture Bun bytes or pin changed")
    expected = {name: file_identity(receipt_path.parent / name, limit) for name, limit in FILES.items()}
    if receipt["files"] != expected:
        raise ValueError("rich fixture artifact changed")
    return {"candidate": product, "prepared": receipt,
            "receipt_sha256": native_candidate.hash_file(receipt_path)}
