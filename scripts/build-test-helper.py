#!/usr/bin/env python3
"""Build the native sandbox test prerequisite and publish its immutable artifact receipt."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import stat
import tempfile
import shutil
import sys

ROOT = Path(__file__).resolve().parents[1]
BINARY = "rw-sandbox-helper"
ENVIRONMENT_KEY = "ROTTWEILER_TEST_SANDBOX_HELPER_RECEIPT"


def build() -> Path:
    command = ["cargo", "build", "--locked", "--all-features", "-p", "rw-sandbox",
               "--bin", BINARY, "--message-format=json-render-diagnostics"]
    executable = None
    # Inherit this worktree's target/profile and stream Cargo output. The helper
    # is a test prerequisite; acceptance measurements never compile it implicitly.
    with subprocess.Popen(command, cwd=ROOT, stdout=subprocess.PIPE, text=True) as process:
        assert process.stdout is not None
        for line in process.stdout:
            message = json.loads(line)
            if (message.get("reason") == "compiler-artifact"
                    and message.get("target", {}).get("name") == BINARY
                    and "bin" in message.get("target", {}).get("kind", [])
                    and message.get("executable")):
                executable = Path(message["executable"])
        if process.wait() != 0:
            raise RuntimeError("sandbox test helper build failed")
    if executable is None or not executable.is_file() or not os.access(executable, os.X_OK):
        raise RuntimeError("Cargo did not produce an executable sandbox test helper")
    return executable.resolve(strict=True)


def write_receipt(executable: Path) -> Path:
    """Publish independent bytes so later Cargo feature builds cannot replace them."""
    executable = executable.resolve(strict=True)
    base = executable.parent / ".rw-test-helpers"
    base.mkdir(mode=0o700, exist_ok=True)
    if base.is_symlink() or not base.is_dir():
        raise RuntimeError("sandbox helper snapshot directory is invalid")
    temporary = Path(tempfile.mkdtemp(prefix=".building-", dir=base))
    try:
        snapshot = temporary / BINARY
        with executable.open("rb") as source, snapshot.open("xb") as output:
            before = os.fstat(source.fileno())
            if (not stat.S_ISREG(before.st_mode) or before.st_size <= 0
                    or before.st_size > 256 * 1024 * 1024 or before.st_mode & 0o111 == 0):
                raise RuntimeError("sandbox helper artifact size or mode is invalid")
            digest = hashlib.sha256()
            copied = 0
            while chunk := source.read(64 * 1024):
                copied += len(chunk)
                if copied > before.st_size:
                    raise RuntimeError("sandbox helper changed while copying its bytes")
                digest.update(chunk)
                output.write(chunk)
            output.flush()
            os.fsync(output.fileno())
            after = os.fstat(source.fileno())
        fields = ("st_dev", "st_ino", "st_size", "st_mtime_ns", "st_ctime_ns")
        if copied != before.st_size or any(getattr(before, key) != getattr(after, key) for key in fields):
            raise RuntimeError("sandbox helper changed while producing its receipt")
        snapshot.chmod(0o500)
        generation = base / digest.hexdigest()
        if not generation.exists() and not generation.is_symlink():
            temporary.rename(generation)
        if generation.is_symlink() or not generation.is_dir():
            raise RuntimeError("sandbox helper snapshot generation is invalid")
        snapshot = generation / BINARY
        if snapshot.is_symlink():
            raise RuntimeError("sandbox helper snapshot must be a regular file")
        with snapshot.open("rb") as source:
            metadata = os.fstat(source.fileno())
            if (not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1
                    or metadata.st_size != copied or metadata.st_mode & 0o777 != 0o500
                    or hashlib.file_digest(source, "sha256").hexdigest() != digest.hexdigest()):
                raise RuntimeError("sandbox helper snapshot identity does not match approved bytes")
        receipt = generation / (BINARY + ".identity.json")
        body = {"executable": str(snapshot), "device": metadata.st_dev,
                "inode": metadata.st_ino, "bytes": metadata.st_size, "sha256": digest.hexdigest()}
        encoded = json.dumps(body, separators=(",", ":")) + "\n"
        if receipt.exists():
            if receipt.is_symlink() or receipt.read_text() != encoded:
                raise RuntimeError("sandbox helper snapshot receipt identity is invalid")
        else:
            with receipt.open("x") as output:
                output.write(encoded)
        return receipt
    finally:
        if temporary.exists():
            shutil.rmtree(temporary)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--github-env", type=Path)
    args = parser.parse_args()
    receipt = write_receipt(build())
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
