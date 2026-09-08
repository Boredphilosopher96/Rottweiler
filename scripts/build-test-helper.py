#!/usr/bin/env python3
"""Build and atomically publish the sandbox helper and native ownership fixture."""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import errno
import hashlib
import json
import io
import os
from pathlib import Path
import shutil
import stat
import sys
import tempfile
import tomllib

import native_candidate
from perf_process import run_sample

ROOT = Path(__file__).resolve().parents[1]
BINARY = "rw-sandbox-helper"
FIXTURE = "rw-sandbox-ownership-fixture"
BINARIES = (BINARY, FIXTURE)
ENVIRONMENT_KEY = "ROTTWEILER_TEST_SANDBOX_HELPER_RECEIPT"


def dev_profile(environment: dict[str, str]) -> dict:
    """Read the workspace's native debug profile, including explicit env overrides."""
    profile = tomllib.loads((ROOT / "Cargo.toml").read_text()).get("profile", {}).get("dev", {})
    defaults = {"opt-level": 0, "debug": 2, "debug-assertions": True, "overflow-checks": True}
    values = {key: environment.get("CARGO_PROFILE_DEV_" + key.upper().replace("-", "_"),
                                  profile.get(key, default)) for key, default in defaults.items()}
    for key in ("debug-assertions", "overflow-checks"):
        if values[key] in ("true", "false"):
            values[key] = values[key] == "true"
        if type(values[key]) is not bool:
            raise ValueError("sandbox prerequisite profile requires boolean checks")
    debug = str(values["debug"]).lower()
    levels = {"false": 0, "none": 0, "0": 0, "limited": 1, "1": 1,
              "true": 2, "full": 2, "2": 2, "line-tables-only": "line-tables-only",
              "line-directives-only": "line-directives-only"}
    if debug not in levels or str(values["opt-level"]) not in {"0", "1", "2", "3", "s", "z"}:
        raise ValueError("sandbox prerequisite profile is unsupported")
    return {"opt_level": str(values["opt-level"]), "debuginfo": levels[debug],
            "debug_assertions": values["debug-assertions"],
            "overflow_checks": values["overflow-checks"], "test": False}


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("Cargo JSON message has duplicate keys")
        result[key] = value
    return result


def cargo_artifacts(code: int, stdout: bytes, target: Path, profile: dict) -> dict[str, Path]:
    """Select two exact native artifacts from one completed Cargo invocation."""
    executables = {}
    finished = []
    manifest = (ROOT / "crates/rw-sandbox/Cargo.toml").resolve(strict=True)
    sources = {BINARY: manifest.parent / "src/bin/rw-sandbox-helper.rs",
               FIXTURE: manifest.parent / "tests/fixtures/ownership.rs"}
    target = target.resolve(strict=True)
    stream = io.BytesIO(stdout)
    while line := stream.readline(1024 * 1024 + 1):
        if len(line) > 1024 * 1024:
            raise ValueError("Cargo JSON message exceeds 1 MiB")
        event = json.loads(line, object_pairs_hook=unique_object)
        if not isinstance(event, dict):
            raise ValueError("Cargo output requires object messages")
        if finished:
            raise ValueError("Cargo emitted output after build-finished")
        if event.get("reason") == "build-finished":
            finished.append(event.get("success") is True)
            continue
        description = event.get("target", {})
        if not isinstance(description, dict):
            raise ValueError("Cargo artifact target must be an object")
        name = description.get("name")
        if event.get("reason") != "compiler-artifact" or name not in BINARIES:
            continue
        actual = event.get("profile", {})
        if (not isinstance(actual, dict) or any(type(actual.get(key)) is not type(value)
                or actual.get(key) != value for key, value in profile.items())):
            raise ValueError("sandbox prerequisite artifact has the wrong profile")
        if (name in executables or description.get("kind") != ["bin"]
                or description.get("crate_types") != ["bin"]
                or Path(event["manifest_path"]).resolve(strict=True) != manifest
                or Path(description["src_path"]).resolve(strict=True) != sources[name].resolve(strict=True)):
            raise ValueError("sandbox prerequisite artifact is duplicated or has the wrong source")
        path = Path(event["executable"])
        if path.is_symlink():
            raise ValueError("sandbox prerequisite artifact cannot be a symlink")
        executable = path.resolve(strict=True)
        if executable.parent != target or executable.name != name or not executable.is_file():
            raise ValueError("sandbox prerequisite artifact is outside the requested target")
        executables[name] = executable
    if code != 0 or finished != [True] or set(executables) != set(BINARIES):
        raise ValueError("sandbox prerequisite requires exactly two artifacts and one successful Cargo completion")
    return executables


def build() -> dict[str, Path]:
    environment = dict(os.environ)
    target = Path(environment.get("CARGO_TARGET_DIR", ROOT / "target"))
    target = (ROOT / target).resolve() if not target.is_absolute() else target.resolve()
    target_triple = environment.get("CARGO_BUILD_TARGET")
    if target_triple is not None and (not target_triple or Path(target_triple).name != target_triple
            or not all(character.isascii() and (character.isalnum() or character in "-_")
                       for character in target_triple)):
        raise ValueError("sandbox prerequisite requires a native target triple")
    artifact_root = target / target_triple / "debug" if target_triple else target / "debug"
    profile = dev_profile(environment)
    identity = native_candidate.source_identity(ROOT)
    configuration = native_candidate.configuration_fingerprints(ROOT)
    command = ["cargo", "build", "--locked", "--all-features", "--target-dir", str(target),
               "-p", "rw-sandbox", "--bin", BINARY, "--bin", FIXTURE,
               "--message-format=json-render-diagnostics"]
    evidence = Path(tempfile.mkdtemp(prefix="rw-test-helper-build-"))
    (evidence / "inputs.json").write_text(json.dumps({"source": identity,
        "configuration": configuration, "profile": profile, "command": command,
        "target": target_triple, "rustflags": environment.get("RUSTFLAGS"),
        "encoded_rustflags": environment.get("CARGO_ENCODED_RUSTFLAGS")}, sort_keys=True) + "\n")
    print(f"sandbox prerequisite build evidence: {evidence}", file=sys.stderr)
    # Parsing never owns a live Cargo child. Shared cleanup also covers a malformed
    # producer that keeps its pipes open, output floods, and caller cancellation.
    with (evidence / "drain.log").open("wb") as log:
        result = run_sample(command, cwd=ROOT, env=environment, timeout=7200,
                            output_limit=64 * 1024 * 1024, log=log)
    (evidence / "stdout.jsonl").write_bytes(result.stdout)
    (evidence / "stderr.log").write_bytes(result.stderr)
    if (native_candidate.source_identity(ROOT) != identity
            or native_candidate.configuration_fingerprints(ROOT) != configuration):
        raise ValueError("sandbox prerequisite source or Cargo configuration changed during build")
    return cargo_artifacts(result.returncode, result.stdout, artifact_root, profile)


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
    except (OSError, ValueError, RuntimeError, KeyError, TypeError) as error:
        print(f"sandbox test prerequisite: {error}", file=sys.stderr)
        raise SystemExit(1) from error
