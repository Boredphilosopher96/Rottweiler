#!/usr/bin/env python3
"""Exercise compiled App/transport ownership and explicit process handoff without building."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import sys

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO / "scripts"))
import native_candidate
from perf_process import run_sample
from perf_scratch import retained_scratch
from perf_report import read_report, MEMORY_REPORT_BYTES
from release_contract import load_contract

TUI_ROLE = load_contract(REPO / "contracts/release-contract.json").js_host_roles["tui"]



def validate_handoff(data: dict, cycles: int) -> None:
    """Require the actual navigation and attachment oracles, including every retirement."""
    if (data.get("schemaVersion") != 1 or data.get("cycles") != cycles
            or data.get("finalAllocationBytes") != 0 or data.get("resolvedChildControls") != 0
            or data.get("handoffAttachmentBytes", 0) <= 4 * 1024 * 1024):
        raise ValueError("compiled probe lacks complete pending-control/attachment ownership proof")
    history = data.get("history", {})
    expected = [("earliest", "0"), ("middle", "5000"), ("append-away", "5000"),
                ("resize", "5000"), ("latest", "10000"),
                ("evicted-middle-after-reconnect", "5000"), ("latest-after-reconnect", "10000"), ("search-match", "5001")]
    observations = history.get("observations", [])
    if (history.get("initialRows") != 10_000 or history.get("finalRows") != 10_001
            or history.get("mixedKinds") != ["user", "assistant-markdown-code", "tool"]
            or [(item.get("stage"), item.get("anchor")) for item in observations] != expected
            or any(not 0 < item.get("mounted", 0) <= 16 or item.get("cacheBytes", 0) <= 0 for item in observations)):
        raise ValueError("compiled probe lacks exact mixed-history navigation proof")
    destroyed = [sample for sample in data.get("samples", []) if sample.get("stage") == "destroyed"]
    if ([sample.get("cycle") for sample in destroyed] != list(range(cycles))
            or any(sample.get("allocation", {}).get("bytes") != 0 for sample in destroyed)):
        raise ValueError("compiled probe omitted physical teardown observations")


def run(candidate: Path, output: Path, cycles: int, generations: int, collect: bool = False) -> None:
    receipt = native_candidate.verify(candidate, REPO)
    executable = candidate / receipt["components"]["js_host"]["path"]
    output.mkdir(parents=True, exist_ok=False)
    reports = []
    with retained_scratch("rw-client-memory-", parent=Path("/tmp"),
                          evidence=lambda path: (output / "failed-scratch.txt").write_text(str(path) + "\n")) as private:
        for generation in range(generations):
            report = output / f"process-{generation}.json"
            recycle = generation + 1 < generations
            environment = dict(probe_environment(), ROTTWEILER_HOME=str(private / "home"),
                               ROTTWEILER_CLIENT_MEMORY_PROBE_REPORT=str(report),
                               ROTTWEILER_CLIENT_MEMORY_PROBE_DIRECTORY=str(private),
                               ROTTWEILER_CLIENT_MEMORY_PROBE_CYCLES=str(cycles),
                               ROTTWEILER_CLIENT_MEMORY_COLLECT="1" if collect else "0",
                               ROTTWEILER_CLIENT_MEMORY_PROBE_RECYCLE="1" if recycle else "0")
            try:
                with (output / f"process-{generation}.log").open("wb") as log:
                    result = run_sample([str(executable), TUI_ROLE], cwd=private, env=environment,
                                        log=log, output_limit=2 * 1024 * 1024, timeout=180)
            finally:
                if native_candidate.verify(candidate, REPO) != receipt:
                    raise ValueError("candidate changed during compiled memory probe")
            if result.returncode != (75 if recycle else 0):
                raise ValueError(f"compiled memory probe generation {generation} exited {result.returncode}; see its log")
            data = read_report(report, MEMORY_REPORT_BYTES)
            if data.get("collection") != ("forced-after-cycle" if collect else "production-policy"):
                raise ValueError("probe garbage collection mode differs")
            validate_handoff(data, cycles)
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


def run_held(candidate: Path, output: Path, cycles: int, view: str, collect: bool = False) -> None:
    receipt = native_candidate.verify(candidate, REPO)
    executable = candidate / receipt["components"]["js_host"]["path"]
    output.mkdir(parents=True, exist_ok=False)
    report = output / f"held-{view}.json"
    with retained_scratch("rw-held-memory-", parent=Path("/tmp"),
                          evidence=lambda path: (output / "failed-scratch.txt").write_text(str(path) + "\n")) as private:
        environment = dict(probe_environment(), ROTTWEILER_HOME=str(private / "home"),
                           ROTTWEILER_CLIENT_MEMORY_PROBE_REPORT=str(report),
                           ROTTWEILER_CLIENT_MEMORY_PROBE_DIRECTORY=str(private),
                           ROTTWEILER_CLIENT_MEMORY_PROBE_CYCLES=str(cycles),
                               ROTTWEILER_CLIENT_MEMORY_COLLECT="1" if collect else "0",
                           ROTTWEILER_CLIENT_MEMORY_HELD_VIEW=view)
        try:
            with (output / f"held-{view}.log").open("wb") as log:
                result = run_sample([str(executable), TUI_ROLE], cwd=private, env=environment,
                                    log=log, output_limit=2 * 1024 * 1024, timeout=300)
        finally:
            if native_candidate.verify(candidate, REPO) != receipt:
                raise ValueError("candidate changed during held-view probe")
        if result.returncode != 0:
            raise ValueError(f"held {view} probe exited {result.returncode}; see its log")
        data = read_report(report, MEMORY_REPORT_BYTES)
        if data.get("collection") != ("forced-every-ten-cycles" if collect else "production-policy"):
            raise ValueError("held probe garbage collection mode differs")
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
            if not key.startswith(("ROTTWEILER_", "OTUI_", "OPENTUI_")) and key != "BUN_OPTIONS"}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cycles", type=int, default=20)
    parser.add_argument("--generations", type=int, default=3)
    parser.add_argument("--collect-garbage", action="store_true", help="Explicit allocator diagnostic; default retains the production collection policy without harness collection")
    parser.add_argument("--held-view", choices=["output", "review", "secret", "action"])
    args = parser.parse_args()
    maximum_cycles = 1000 if args.held_view is not None else 200
    if not 1 <= args.cycles <= maximum_cycles or not 1 <= args.generations <= 10:
        parser.error(f"cycles must be 1..{maximum_cycles} and generations 1..10")
    if args.held_view is not None:
        run_held(args.candidate.resolve(), args.output.resolve(), args.cycles, args.held_view, args.collect_garbage)
    else:
        run(args.candidate.resolve(), args.output.resolve(), args.cycles, args.generations, args.collect_garbage)


if __name__ == "__main__":
    main()
