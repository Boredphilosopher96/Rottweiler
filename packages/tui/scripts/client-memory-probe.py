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


def run(candidate: Path, output: Path, cycles: int, generations: int) -> None:
    receipt = native_candidate.verify(candidate, REPO)
    executable = candidate / receipt["components"]["tui"]["path"]
    output.mkdir(parents=True, exist_ok=True)
    reports = []
    with tempfile.TemporaryDirectory(prefix="rw-client-memory-", dir="/tmp") as temporary:
        private = Path(temporary)
        for generation in range(generations):
            report = output / f"process-{generation}.json"
            recycle = generation + 1 < generations
            environment = dict(os.environ, ROTTWEILER_HOME=str(private / "home"),
                               ROTTWEILER_CLIENT_MEMORY_PROBE_REPORT=str(report),
                               ROTTWEILER_CLIENT_MEMORY_PROBE_DIRECTORY=str(private),
                               ROTTWEILER_CLIENT_MEMORY_PROBE_CYCLES=str(cycles),
                               ROTTWEILER_CLIENT_MEMORY_PROBE_RECYCLE="1" if recycle else "0")
            with (output / f"process-{generation}.log").open("wb") as log:
                result = subprocess.run([str(executable)], cwd=private, env=environment,
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


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cycles", type=int, default=20)
    parser.add_argument("--generations", type=int, default=3)
    args = parser.parse_args()
    if not 1 <= args.cycles <= 200 or not 1 <= args.generations <= 10:
        parser.error("cycles must be 1..200 and generations 1..10")
    run(args.candidate.resolve(), args.output.resolve(), args.cycles, args.generations)


if __name__ == "__main__":
    main()
