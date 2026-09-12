#!/usr/bin/env python3
"""Explicitly compile the SDK rich fixture before any native acceptance execution."""
from __future__ import annotations
import argparse
import json
import os
from pathlib import Path
import tempfile
import native_candidate
import rich_fixture_inputs as inputs
from perf_process import run_sample

REPO = Path(__file__).resolve().parents[1]


def prepare(candidate: Path, destination: Path, bun: Path) -> Path:
    candidate, destination, bun = candidate.resolve(strict=True), destination.absolute(), bun.resolve(strict=True)
    product = native_candidate.verify(candidate, REPO)
    if native_candidate.source_identity(REPO) != product["identity"]["source"]:
        raise ValueError("rich preparation source differs from candidate")
    if destination.exists():
        existing = inputs.verify(candidate, destination / inputs.RECEIPT, REPO)
        if existing["prepared"]["bun"]["path"] != str(bun):
            raise ValueError("requested Bun differs from existing rich preparation")
        return destination / inputs.RECEIPT
    expected = (REPO / ".bun-version").read_text().strip()
    identity = inputs.file_identity(bun, 256 * 1024 * 1024)
    env = dict(os.environ)
    version = run_sample([str(bun), "--version"], cwd=REPO, env=env, timeout=5)
    if version.returncode or version.stdout.decode().strip() != expected:
        raise ValueError("rich preparation requires pinned Bun")
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".rich-prepare-", dir=destination.parent) as temporary:
        staging = Path(temporary)
        source = REPO / "packages/plugin-sdk/fixtures/conformance/rich-workflow.ts"
        with (destination.parent / (destination.name + "-build.log")).open("wb") as log:
            result = run_sample([str(bun), "build", "--target=bun", str(source), "--outfile", str(staging / "plugin.js")],
                                cwd=REPO, env=env, timeout=60, output_limit=2 * 1024 * 1024, log=log)
        if result.returncode:
            raise ValueError("rich SDK fixture build failed")
        result = run_sample([str(bun), str(staging / "plugin.js"), "--manifest"], cwd=REPO, env=env,
                            timeout=5, output_limit=64 * 1024)
        if result.returncode:
            raise ValueError("rich SDK manifest construction failed")
        (staging / "manifest.json").write_bytes(result.stdout)
        receipt = {"schema_version": 1, "candidate_identity": product["identity_sha256"],
                   "source": product["identity"]["source"], "bun": {"path": str(bun), "version": expected, **identity},
                   "files": {name: inputs.file_identity(staging / name, limit) for name, limit in inputs.FILES.items()}}
        (staging / inputs.RECEIPT).write_text(json.dumps(receipt, sort_keys=True) + "\n")
        for path in staging.iterdir():
            path.chmod(0o400)
        inputs.verify(candidate, staging / inputs.RECEIPT, REPO)
        if native_candidate.source_identity(REPO) != product["identity"]["source"]:
            raise ValueError("rich source changed during preparation")
        os.rename(staging, destination)
    return destination / inputs.RECEIPT


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--bun", type=Path, required=True)
    args = parser.parse_args()
    print(prepare(args.candidate, args.output, args.bun))
