#!/usr/bin/env python3
"""Build and publish the MCP fixture before native acceptance or conditioning."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

import m8_inputs
import native_candidate
import native_profile

REPO = Path(__file__).resolve().parents[1]


def prepare(candidate: Path, output: Path, target_dir: Path) -> Path:
    product = native_candidate.verify(candidate, REPO)
    if output.exists():
        m8_inputs.verify(candidate, output / m8_inputs.RECEIPT, REPO)
        return output / m8_inputs.RECEIPT
    identity = native_candidate.build_identity(REPO)
    if identity != product["identity"]:
        raise ValueError("M8 fixture build environment differs from candidate preparation")
    environment = dict(os.environ, CARGO_TARGET_DIR=str(target_dir.resolve()))
    command = [str(REPO / "scripts/cargo-release.sh"), "build", "--locked", "--release",
               "-p", "rw-mcp", "--features", "rw-mcp/test-support", "--bin", m8_inputs.FIXTURE,
               "--message-format=json-render-diagnostics"]
    executable = None
    with subprocess.Popen(command, cwd=REPO, env=environment, stdout=subprocess.PIPE, text=True) as process:
        for line in process.stdout:
            event = json.loads(line)
            target = event.get("target", {})
            if (event.get("reason") == "compiler-artifact" and target.get("name") == m8_inputs.FIXTURE
                    and "bin" in target.get("kind", []) and event.get("executable")):
                executable = Path(event["executable"]).resolve(strict=True)
        if process.wait() != 0 or executable is None:
            raise RuntimeError("M8 fixture Cargo build did not produce the required executable")
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
