#!/usr/bin/env python3
"""Build, size-check, and atomically publish one reusable native candidate."""
from __future__ import annotations

import argparse
import contextlib
import fcntl
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

import artifact_bundle
import ci_inventory
import native_candidate
from release_contract import load_contract, stage_release, verify_archive

REPO = Path(__file__).resolve().parents[1]


def run(command: list[str], environment: dict[str, str]) -> None:
    subprocess.run(command, cwd=REPO, env=environment, stdout=sys.stderr, check=True)


def build(base: Path, target: Path) -> Path:
    identity = native_candidate.build_identity(REPO)
    key = native_candidate.identity_key(identity)
    base.mkdir(parents=True, exist_ok=True)
    if base.is_symlink() or not base.is_dir():
        raise ValueError("candidate output must be a real directory")
    destination = base / key
    # Publication is serialized within this worktree; Cargo targets are never
    # shared between source trees. Keep the descriptor through publication.
    lock_path = base / ".build.lock"
    descriptor = os.open(lock_path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "r+") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if destination.exists():
            native_candidate.verify(destination, REPO, expected_identity=identity)
            return destination
        environment = dict(os.environ, CARGO_TARGET_DIR=str(target), CARGO_PROFILE_RELEASE_DEBUG="0")
        if not environment.get("SOURCE_DATE_EPOCH"):
            environment["SOURCE_DATE_EPOCH"] = native_candidate.output(["git", "show", "-s", "--format=%ct", "HEAD"], REPO)
        # Separate Cargo invocations keep the WASM helper's dependency features
        # out of the public entrypoint.
        for package in ("rw-cli", "rw-wasm-host"):
            run(["scripts/cargo-release.sh", "build", "--locked", "--release", "-p", package], environment)
        packages = [package for package in ci_inventory.inventory(REPO)["packages"] if "native_component" in package]
        with contextlib.redirect_stdout(sys.stderr):
            ci_inventory.install(REPO, [package["id"] for package in packages], build_dependencies=True, stdout=sys.stderr)
        for package in packages:
            run(["bun", "run", "--cwd", package["directory"], "build"], environment)
        if native_candidate.source_identity(REPO) != identity["source"]:
            raise ValueError("source changed during candidate build")
        release_directory = Path(subprocess.check_output(
            ["scripts/cargo-release.sh", "artifact-dir"], cwd=REPO, env=environment, text=True
        ).strip())
        contract = load_contract(REPO / "contracts/release-contract.json")
        platform = contract.platform(identity["platform"])
        root_name = contract.archive_root(identity["version"], platform.id)
        temporary = Path(tempfile.mkdtemp(prefix=".building-", dir=base))
        try:
            stage = temporary / root_name
            stage_release(contract, stage, REPO / "scripts/install-release.sh", identity["version"], platform.id,
                          release_directory / "rw", release_directory / "rottweiler-wasm-host",
                          REPO / "packages/js-host/dist/rottweiler-js-host",
                          REPO / "packages/js-host/dist" / platform.native_library)
            archive = temporary / f"{root_name}.tar.gz"
            run([sys.executable, "scripts/package-release.py", str(stage), str(archive)], environment)
            verify_archive(contract, archive, identity["version"], platform.id)
            relative_paths = {member.id: f"{root_name}/{member.path}" for member in platform.archive_members}
            relative_paths["archive"] = archive.name
            components = {
                name: {"path": relative, "bytes": (temporary / relative).stat().st_size,
                       "sha256": native_candidate.hash_file(temporary / relative)}
                for name, relative in relative_paths.items()
            }
            receipt = {
                "schema_version": 1, "identity": identity, "identity_sha256": key,
                "origin": {name: os.environ.get(name) for name in
                           ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_JOB", "RUNNER_OS", "RUNNER_ARCH", "ImageOS", "ImageVersion")},
                "components": components,
            }
            (temporary / native_candidate.RECEIPT).write_text(json.dumps(receipt, sort_keys=True, indent=2) + "\n")
            (temporary / artifact_bundle.MANIFEST).write_text(json.dumps(
                artifact_bundle.document(temporary, identity["source"]["commit"], platform.id), sort_keys=True
            ) + "\n")
            native_candidate.verify(temporary, REPO, expected_identity=identity)
            with tempfile.TemporaryDirectory(prefix=".verify-", dir=base) as extracted:
                run(["tar", "-xzf", str(archive), "-C", extracted], environment)
                run([str(Path(extracted) / relative_paths["engine"]), "--version"], environment)
                host_identity = subprocess.check_output([str(Path(extracted) / relative_paths["js_host"]), contract.js_host_roles["source_plugin"], "version"], env=environment, text=True)
                host = json.loads(host_identity)
                if set(host) != {"abi", "format"} or not isinstance(host["abi"], int) or host["abi"] < 1 or not isinstance(host["format"], str) or not host["format"]:
                    raise ValueError("candidate plugin host reported an invalid semantic identity")
            os.rename(temporary, destination)
        finally:
            if temporary.exists():
                shutil.rmtree(temporary)
        return destination


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=REPO / "dist/candidates")
    parser.add_argument("--target-dir", type=Path, default=Path(os.environ.get("CARGO_TARGET_DIR", str(REPO / "target"))))
    parser.add_argument("--print", choices=("candidate", "archive"), default="candidate")
    parser.add_argument("--github-output", type=Path)
    args = parser.parse_args()
    candidate = build(args.output.absolute(), args.target_dir.absolute())
    if args.github_output is not None:
        receipt = native_candidate.read_receipt(candidate)
        archive = candidate / receipt["components"]["archive"]["path"]
        if any(character in str(candidate) for character in "\r\n"):
            raise ValueError("candidate path cannot contain a line break")
        with args.github_output.open("a") as stream:
            stream.write(f"candidate={candidate}\narchive={archive}\n")
    print(native_candidate.component_path(candidate, REPO, "archive") if args.print == "archive" else candidate)


if __name__ == "__main__":
    main()
