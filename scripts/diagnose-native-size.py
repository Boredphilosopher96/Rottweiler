#!/usr/bin/env python3
"""Preserve a failed native ELF and explicitly relink its final target for a map.

This diagnostic never publishes a candidate or changes a gate's result. It runs
only after the normal native build has failed, outside all measurement intervals.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil

import ci_evidence
import native_candidate


def describe(path: Path) -> dict:
    return {"path": str(path), "bytes": path.stat().st_size,
            "sha256": native_candidate.hash_file(path)}


def diagnose(repo: Path, target_dir: Path, output: Path, failed_gate: Path) -> int:
    original_gate = json.loads(failed_gate.read_text())
    if original_gate.get("status") != "failed" or original_gate.get("exit_code", 0) == 0:
        raise ValueError("native size diagnostics require a failed build gate")
    identity = native_candidate.build_identity(repo)
    if not identity["platform"].startswith("linux-") or "-linux-" not in identity["target"]:
        raise ValueError("ELF link-map diagnostic requires a native Linux build")
    if original_gate.get("source_sha") != identity["source"]["commit"]:
        raise ValueError("failed build source differs from the diagnostic checkout")
    executable = target_dir / identity["target"] / "release" / "rw"
    if executable.is_symlink() or not executable.is_file():
        raise ValueError("failed native build has no regular engine artifact")
    with executable.open("rb") as stream:
        if stream.read(4) != b"\x7fELF":
            raise ValueError("failed engine artifact is not ELF")
    output.mkdir(parents=True, exist_ok=False)
    preserved = output / "failed-engine.elf"
    original = describe(executable)
    shutil.copyfile(executable, preserved)
    preserved.chmod(0o500)
    snapshot = describe(preserved)
    if original["sha256"] != snapshot["sha256"] or describe(executable) != original:
        raise ValueError("native engine changed while preserving its failure evidence")
    # Preserve the actual failing artifact before diagnostic codegen can replace it.
    document = {"identity": identity, "failed_gate": original_gate,
                "original": original, "preserved": snapshot}
    manifest = output / "failure.json"
    manifest.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n")
    command = ["cargo", "rustc", "--locked", "--release", "--target", identity["target"],
               "--target-dir", str(target_dir), "-p", "rw-cli", "--bin", "rw", "--",
               "-C", "link-arg=-Wl,-Map=" + str(output / "engine.map")]
    document["diagnostic_command"] = command
    manifest.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n")
    # Match cargo-release.sh and the builder exactly; only this final target's
    # rustc arguments change. Dependent crates retain their existing build graph.
    os.environ["CARGO_TARGET_DIR"] = str(target_dir)
    os.environ["CARGO_PROFILE_RELEASE_OPT_LEVEL"] = identity["profile"]["opt_level"]
    os.environ["CARGO_PROFILE_RELEASE_DEBUG"] = "0"
    if not os.environ.get("SOURCE_DATE_EPOCH"):
        os.environ["SOURCE_DATE_EPOCH"] = native_candidate.output(
            ["git", "show", "-s", "--format=%ct", "HEAD"], repo)
    status = ci_evidence.observe(command, "native-link-map", output / "relink.json")
    document["diagnostic_exit_code"] = status
    document["source_after"] = native_candidate.source_identity(repo)
    if executable.is_file():
        document["diagnostic_engine"] = describe(executable)
    if (output / "engine.map").is_file():
        document["link_map"] = describe(output / "engine.map")
    manifest.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n")
    if document["source_after"] != identity["source"]:
        raise ValueError("source changed during diagnostic relink")
    return status


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--failed-gate", type=Path, required=True)
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[1]
    os.chdir(repo)
    return diagnose(repo, args.target_dir.resolve(strict=True), args.output.absolute(),
                    args.failed_gate.resolve(strict=True))


if __name__ == "__main__":
    raise SystemExit(main())
