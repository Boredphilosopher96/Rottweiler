#!/usr/bin/env python3
"""Build and publish the MCP fixture before native acceptance or conditioning."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import tempfile

import m8_inputs
import native_candidate
import native_profile
from perf_process import run_sample

REPO = Path(__file__).resolve().parents[1]


def cargo_artifact(code: int, stdout: bytes, target_dir: Path, target: str) -> Path:
    artifacts = []
    finished = []
    for line in stdout.splitlines():
        event = json.loads(line)
        if not isinstance(event, dict):
            raise ValueError("Cargo output must contain object messages")
        if event.get("reason") == "build-finished":
            finished.append(event.get("success"))
        description = event.get("target", {})
        if (event.get("reason") != "compiler-artifact" or description.get("name") != m8_inputs.FIXTURE
                or description.get("kind") != ["bin"]):
            continue
        profile = event.get("profile", {})
        if (profile.get("opt_level") != native_profile.settings(target, REPO)["opt_level"]
                or profile.get("debuginfo") not in (None, 0)
                or profile.get("debug_assertions") is not False
                or profile.get("overflow_checks") is not False or profile.get("test") is not False):
            raise ValueError("M8 fixture artifact does not use the native release profile")
        executable = Path(event["executable"]).resolve(strict=True)
        root = target_dir.resolve(strict=True)
        if (not executable.is_relative_to(root) or executable.name != m8_inputs.FIXTURE
                or executable.parent.name != "release" or not executable.is_file()):
            raise ValueError("M8 fixture artifact is outside its requested release target")
        artifacts.append(executable)
    if code != 0 or finished != [True] or len(artifacts) != 1:
        raise ValueError("M8 fixture requires one artifact and one successful Cargo completion")
    return artifacts[0]


def prepare(candidate: Path, output: Path, target_dir: Path) -> Path:
    product = native_candidate.verify(candidate, REPO)
    identity = native_candidate.build_identity(REPO)
    if identity != product["identity"]:
        raise ValueError("M8 fixture build environment differs from candidate preparation")
    if output.exists():
        m8_inputs.verify(candidate, output / m8_inputs.RECEIPT, REPO)
        return output / m8_inputs.RECEIPT
    environment = dict(os.environ, CARGO_TARGET_DIR=str(target_dir.resolve()))
    command = [str(REPO / "scripts/cargo-release.sh"), "build", "--locked", "--release",
               "-p", "rw-mcp", "--features", "rw-mcp/test-support", "--bin", m8_inputs.FIXTURE,
               "--message-format=json-render-diagnostics"]
    output.parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix=".m8-build-", dir=output.parent))
    # The shared owner settles Cargo's group before parsing any compiler output.
    # Partial output survives timeout/flood/cancellation in the bounded drain log.
    with (evidence / "drain.log").open("wb") as log:
        result = run_sample(command, cwd=REPO, env=environment, timeout=7200,
                            output_limit=64 * 1024 * 1024, log=log)
    (evidence / "stdout.jsonl").write_bytes(result.stdout)
    (evidence / "stderr.log").write_bytes(result.stderr)
    executable = cargo_artifact(result.returncode, result.stdout, target_dir, identity["target"])
    if native_candidate.verify(candidate, REPO) != product:
        raise ValueError("candidate changed during M8 fixture preparation")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".m8-fixture-", dir=output.parent) as temporary:
        staging = Path(temporary)
        shutil.copyfile(executable, staging / m8_inputs.FIXTURE)
        (staging / m8_inputs.FIXTURE).chmod(0o500)
        receipt = {"schema_version": 1, "candidate_identity": product["identity_sha256"],
                   "source": identity["source"], "target": identity["target"],
                   "rust": identity["toolchains"]["rust"],
                   "profile": native_profile.settings(identity["target"], REPO),
                   "fixture": m8_inputs.fixture_identity(staging / m8_inputs.FIXTURE)}
        (staging / m8_inputs.RECEIPT).write_text(json.dumps(receipt, sort_keys=True) + "\n")
        (staging / m8_inputs.RECEIPT).chmod(0o400)
        m8_inputs.verify(candidate, staging / m8_inputs.RECEIPT, REPO)
        os.rename(staging, output)
    return output / m8_inputs.RECEIPT


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--target-dir", type=Path, required=True)
    parser.add_argument("--github-output", type=Path)
    args = parser.parse_args()
    receipt = prepare(args.candidate.resolve(strict=True), args.output.absolute(), args.target_dir)
    if args.github_output:
        with args.github_output.open("a") as output:
            output.write(f"executable={receipt.parent / m8_inputs.FIXTURE}\nreceipt={receipt}\n")
    print(receipt)


if __name__ == "__main__":
    main()
