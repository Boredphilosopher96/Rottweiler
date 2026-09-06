#!/usr/bin/env python3
"""Measure a verified compiled App's near-limit key-to-frame latency without building."""
from __future__ import annotations

import argparse
import json
import math
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO / "scripts"))
import native_candidate
from release_contract import load_contract

TUI_ROLE = load_contract(REPO / "contracts/release-contract.json").js_host_roles["tui"]


def validate(data: dict) -> None:
    """Recompute the gate from raw trials; do not trust a reported passing flag."""
    if (data.get("schemaVersion") != 1 or data.get("keysPerTrial") != 128
            or data.get("warmupKeysExcludedPerTrial") != 5 or data.get("budgetMs") != 16
            or data.get("width") != 110 or data.get("height") != 36):
        raise ValueError("input probe measurement contract differs")
    trials = data.get("trials", [])
    if len(trials) != 3:
        raise ValueError("input probe requires three complete raw trials")
    maximum = data.get("maximumComposerUtf8Bytes")
    if type(maximum) is not int or maximum < 256:
        raise ValueError("input probe has no admitted composer limit")
    for trial in trials:
        samples = trial.get("samplesMs", [])
        if len(samples) != 128 or any(type(value) not in (int, float)
                                     or not math.isfinite(value) or value < 0 for value in samples):
            raise ValueError("input probe raw samples are incomplete or invalid")
        ordered = sorted(samples[5:])
        p99 = ordered[math.ceil(len(ordered) * .99) - 1]
        if trial.get("p99Ms") != p99 or p99 >= 16:
            raise ValueError("compiled input/render p99 must be below 16ms in every trial")
        if (trial.get("exactContent") is not True or trial.get("nativeFrameContainsInput") is not True
                or trial.get("finalUtf8Bytes") != maximum - 128 or trial.get("allocationBytes", 0) <= 0):
            raise ValueError("input probe lost exact near-limit content or its retention owner")
    terminal = data.get("terminal", {})
    if (data.get("finalAllocationBytes") != 0 or terminal.get("queuedBytes") != 0
            or terminal.get("bytes", 0) <= 0 or data.get("failure") is not None
            or data.get("passed") is not True):
        raise ValueError("input probe did not drain output and retire its allocation")


def run(candidate: Path, output: Path) -> None:
    receipt = native_candidate.verify(candidate, REPO)
    executable = candidate / receipt["components"]["js_host"]["path"]
    output.mkdir(parents=True, exist_ok=True)
    report = output / "input.json"
    # Prevent a failed process from qualifying with evidence left by an earlier run.
    report.unlink(missing_ok=True)
    (output / "summary.json").unlink(missing_ok=True)
    environment = {key: value for key, value in os.environ.items()
                   if not key.startswith("ROTTWEILER_")}
    with tempfile.TemporaryDirectory(prefix="rw-client-input-", dir="/tmp") as temporary:
        private = Path(temporary)
        environment.update(ROTTWEILER_HOME=str(private / "home"),
                           ROTTWEILER_CLIENT_INPUT_PROBE_REPORT=str(report),
                           ROTTWEILER_CLIENT_INPUT_PROBE_DIRECTORY=str(private))
        with (output / "input.log").open("wb") as log:
            result = subprocess.run([str(executable), TUI_ROLE], cwd=private, env=environment,
                                    stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT, timeout=120)
    data = json.loads(report.read_text()) if report.exists() else None
    summary = {"schema_version": 1, "candidate_identity": receipt["identity_sha256"],
               "source": receipt["identity"]["source"], "exit_code": result.returncode,
               "host": {"system": platform.system(), "release": platform.release(), "machine": platform.machine()},
               "qualification": "Compiled near-limit App key-to-native-frame wall clock; startup and RSS are separate gates",
               "process": data}
    (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    if result.returncode != 0 or data is None:
        raise ValueError(f"compiled input probe exited {result.returncode}; inspect input.log and raw input.json")
    validate(data)
    print(json.dumps({key: value for key, value in summary.items() if key != "process"}, sort_keys=True))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    run(args.candidate.resolve(), args.output.resolve())


if __name__ == "__main__":
    main()
