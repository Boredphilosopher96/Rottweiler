#!/usr/bin/env python3
"""Exercise compiled App/transport ownership and explicit process handoff without building."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO / "scripts"))
import native_candidate
from release_contract import load_contract

TUI_ROLE = load_contract(REPO / "contracts/release-contract.json").js_host_roles["tui"]


def run(candidate: Path, output: Path, cycles: int, generations: int) -> None:
    receipt = native_candidate.verify(candidate, REPO)
    executable = candidate / receipt["components"]["js_host"]["path"]
    output.mkdir(parents=True, exist_ok=False)
    reports = []
    with tempfile.TemporaryDirectory(prefix="rw-client-memory-", dir="/tmp") as temporary:
        private = Path(temporary)
        for generation in range(generations):
            report = output / f"process-{generation}.json"
            recycle = generation + 1 < generations
            environment = dict(probe_environment(), ROTTWEILER_HOME=str(private / "home"),
                               ROTTWEILER_CLIENT_MEMORY_PROBE_REPORT=str(report),
                               ROTTWEILER_CLIENT_MEMORY_PROBE_DIRECTORY=str(private),
                               ROTTWEILER_CLIENT_MEMORY_PROBE_CYCLES=str(cycles),
                               ROTTWEILER_CLIENT_MEMORY_PROBE_RECYCLE="1" if recycle else "0")
            with (output / f"process-{generation}.log").open("wb") as log:
                result = subprocess.run([str(executable), TUI_ROLE], cwd=private, env=environment,
                                        stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT, timeout=180)
            if result.returncode != (75 if recycle else 0):
                raise ValueError(f"compiled memory probe generation {generation} exited {result.returncode}; see its log")
            data = json.loads(report.read_text())
            if data["cycles"] != cycles or data["finalAllocationBytes"] != 0:
                raise ValueError("compiled probe did not complete its configured allocation retirement")
            if data["recycle"]["captured"] != recycle or data["recycle"]["restored"] != (generation > 0):
                raise ValueError("compiled probe did not preserve process handoff state")
            reports.append(data)
        if len({report["pid"] for report in reports}) != generations:
            raise ValueError("handoff did not use distinct processes")
    summary = {"schema_version": 1, "candidate_identity": receipt["identity_sha256"],
               "source": receipt["identity"]["source"], "cycles_per_process": cycles, "generations": generations,
               "qualification": "App/transport fixture observations; separate engine+TUI strict RSS and soak gates remain required",
               "max_resident_bytes": max(sample["highWaterBytes"] for report in reports for sample in report["samples"]),
               "processes": reports}
    (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({key: value for key, value in summary.items() if key != "processes"}, sort_keys=True))


def run_held(candidate: Path, output: Path, cycles: int, view: str) -> None:
    receipt = native_candidate.verify(candidate, REPO)
    executable = candidate / receipt["components"]["js_host"]["path"]
    output.mkdir(parents=True, exist_ok=False)
    report = output / f"held-{view}.json"
    with tempfile.TemporaryDirectory(prefix="rw-held-memory-", dir="/tmp") as temporary:
        private = Path(temporary)
        environment = dict(probe_environment(), ROTTWEILER_HOME=str(private / "home"),
                           ROTTWEILER_CLIENT_MEMORY_PROBE_REPORT=str(report),
                           ROTTWEILER_CLIENT_MEMORY_PROBE_DIRECTORY=str(private),
                           ROTTWEILER_CLIENT_MEMORY_PROBE_CYCLES=str(cycles),
                           ROTTWEILER_CLIENT_MEMORY_HELD_VIEW=view)
        with (output / f"held-{view}.log").open("wb") as log:
            result = subprocess.run([str(executable), TUI_ROLE], cwd=private, env=environment,
                                    stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT, timeout=300)
        if result.returncode != 0:
            raise ValueError(f"held {view} probe exited {result.returncode}; see its log")
        data = json.loads(report.read_text())
        if data["cycles"] != cycles or data["view"] != view or data["finalAllocationBytes"] != 0:
            raise ValueError("held-view probe did not complete its admitted lifetime")
    summary = {"schema_version": 1, "candidate_identity": receipt["identity_sha256"],
               "source": receipt["identity"]["source"], "view": view, "cycles": cycles,
               "max_resident_bytes": max(sample["highWaterBytes"] for sample in data["samples"]),
               "qualification": "One mounted view held for all cycles; complete application RSS gate is separate",
               "process": data}
    (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({key: value for key, value in summary.items() if key != "process"}, sort_keys=True))


def probe_environment() -> dict[str, str]:
    return {key: value for key, value in os.environ.items()
            if not key.startswith("ROTTWEILER_")}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cycles", type=int, default=20)
    parser.add_argument("--generations", type=int, default=3)
    parser.add_argument("--held-view", choices=["output", "review", "secret", "action"])
    args = parser.parse_args()
    maximum_cycles = 1000 if args.held_view is not None else 200
    if not 1 <= args.cycles <= maximum_cycles or not 1 <= args.generations <= 10:
        parser.error(f"cycles must be 1..{maximum_cycles} and generations 1..10")
    if args.held_view is not None:
        run_held(args.candidate.resolve(), args.output.resolve(), args.cycles, args.held_view)
    else:
        run(args.candidate.resolve(), args.output.resolve(), args.cycles, args.generations)


if __name__ == "__main__":
    main()
