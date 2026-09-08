#!/usr/bin/env python3
"""Build and atomically publish the sandbox helper and native ownership fixture."""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import errno
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
BINARY = "rw-sandbox-helper"
FIXTURE = "rw-sandbox-ownership-fixture"
BINARIES = (BINARY, FIXTURE)
ENVIRONMENT_KEY = "ROTTWEILER_TEST_SANDBOX_HELPER_RECEIPT"


def build() -> dict[str, Path]:
    command = ["cargo", "build", "--locked", "--all-features", "-p", "rw-sandbox",
               "--bin", BINARY, "--bin", FIXTURE, "--message-format=json-render-diagnostics"]
    executables: dict[str, Path] = {}
    # Select both artifacts from this one Cargo invocation, never a target search.
    with subprocess.Popen(command, cwd=ROOT, stdout=subprocess.PIPE, text=True) as process:
        assert process.stdout is not None
        for line in process.stdout:
            message = json.loads(line)
            target = message.get("target", {})
            if (message.get("reason") == "compiler-artifact"
                    and target.get("name") in BINARIES
                    and "bin" in target.get("kind", []) and message.get("executable")):
                executables[target["name"]] = Path(message["executable"]).resolve(strict=True)
        if process.wait() != 0:
            raise RuntimeError("sandbox test prerequisite build failed")
    if set(executables) != set(BINARIES):
        raise RuntimeError("Cargo did not produce both sandbox test artifacts")
    return executables


def sync_directory(path: Path) -> None:
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


@contextmanager
def regular_file(path: Path):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, "rb") as source:
        if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
            raise RuntimeError("sandbox test artifact must be a regular file")
        yield source


def copy_artifact(executable: Path, snapshot: Path) -> str:
    with regular_file(executable) as source, snapshot.open("xb") as output:
        before = os.fstat(source.fileno())
        if (not stat.S_ISREG(before.st_mode) or before.st_size <= 0
                or before.st_size > 256 * 1024 * 1024 or before.st_mode & 0o111 == 0):
            raise RuntimeError("sandbox test artifact size or mode is invalid")
        digest = hashlib.sha256()
        copied = 0
        while chunk := source.read(64 * 1024):
            copied += len(chunk)
            if copied > before.st_size:
                raise RuntimeError("sandbox test artifact changed while copying")
            digest.update(chunk)
            output.write(chunk)
        os.fchmod(output.fileno(), 0o500)
        output.flush()
        os.fsync(output.fileno())
        after = os.fstat(source.fileno())
    fields = ("st_dev", "st_ino", "st_size", "st_mtime_ns", "st_ctime_ns")
    if copied != before.st_size or any(getattr(before, key) != getattr(after, key) for key in fields):
        raise RuntimeError("sandbox test artifact changed while producing its receipt")
    return digest.hexdigest()


def identity_body(snapshot: Path, published: Path, digest: str) -> str:
    if snapshot.is_symlink():
        raise RuntimeError("sandbox test snapshot must be a regular file")
    with regular_file(snapshot) as source:
        metadata = os.fstat(source.fileno())
        if (not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1
                or metadata.st_size <= 0 or metadata.st_size > 256 * 1024 * 1024
                or metadata.st_mode & 0o777 != 0o500
                or hashlib.file_digest(source, "sha256").hexdigest() != digest):
            raise RuntimeError("sandbox test snapshot identity does not match approved bytes")
    body = {"executable": str(published), "device": metadata.st_dev,
            "inode": metadata.st_ino, "bytes": metadata.st_size, "sha256": digest}
    return json.dumps(body, separators=(",", ":")) + "\n"


def verify_bundle(generation: Path, digests: dict[str, str]) -> None:
    expected = set(BINARIES) | {name + ".identity.json" for name in BINARIES}
    if generation.is_symlink() or not generation.is_dir() or {
            child.name for child in generation.iterdir()} != expected:
        raise RuntimeError("sandbox test snapshot bundle is incomplete or invalid")
    for name in BINARIES:
        snapshot = generation / name
        encoded = identity_body(snapshot, snapshot, digests[name])
        receipt = generation / (name + ".identity.json")
        if receipt.is_symlink() or not receipt.is_file() or receipt.stat().st_size > 4096:
            raise RuntimeError("sandbox test snapshot receipt is invalid")
        with regular_file(receipt) as source:
            if source.read(4097) != encoded.encode():
                raise RuntimeError("sandbox test snapshot receipt identity is invalid")


def write_receipt(executable: Path, fixture: Path) -> Path:
    """Publish helper and fixture bytes with flat receipts in one atomic bundle."""
    inputs = {BINARY: executable.resolve(strict=True), FIXTURE: fixture.resolve(strict=True)}
    base = inputs[BINARY].parent / ".rw-test-helpers"
    base.mkdir(mode=0o700, exist_ok=True)
    if base.is_symlink() or not base.is_dir():
        raise RuntimeError("sandbox test snapshot directory is invalid")
    sync_directory(base.parent)
    temporary = Path(tempfile.mkdtemp(prefix=".building-", dir=base))
    try:
        digests = {name: copy_artifact(inputs[name], temporary / name) for name in BINARIES}
        # Ordered fixed-width digests bind both images in one generation.
        # Each flat receipt names its independently verified image identity.
        generation = base / (digests[BINARY] + "-" + digests[FIXTURE])
        for name in BINARIES:
            encoded = identity_body(temporary / name, generation / name, digests[name])
            with (temporary / (name + ".identity.json")).open("x") as output:
                output.write(encoded)
                output.flush()
                os.fsync(output.fileno())
        sync_directory(temporary)
        if not generation.exists() and not generation.is_symlink():
            try:
                temporary.rename(generation)
            except OSError as error:
                if error.errno not in (errno.EEXIST, errno.ENOTEMPTY):
                    raise
        verify_bundle(generation, digests)
        sync_directory(base)
        return generation / (BINARY + ".identity.json")
    finally:
        if temporary.exists():
            shutil.rmtree(temporary)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--github-env", type=Path)
    args = parser.parse_args()
    artifacts = build()
    receipt = write_receipt(artifacts[BINARY], artifacts[FIXTURE])
    if args.github_env is not None:
        if any(character in str(receipt) for character in "\r\n"):
            raise ValueError("helper executable path cannot contain a line break")
        with args.github_env.open("a") as stream:
            stream.write(f"{ENVIRONMENT_KEY}={receipt}\n")
    print(receipt)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError) as error:
        print(f"sandbox test prerequisite: {error}", file=sys.stderr)
        raise SystemExit(1) from error
