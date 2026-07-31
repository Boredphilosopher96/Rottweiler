#!/usr/bin/env python3
"""Enforce a fail-closed mutation-score floor from cargo-mutants evidence."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


MAX_EVIDENCE_BYTES = 16 * 1024 * 1024
MUTANT_SUMMARIES = {"CaughtMutant", "MissedMutant", "Timeout", "Unviable"}
EVIDENCE_KEYS = {
    "cargo_mutants_version",
    "caught",
    "end_time",
    "missed",
    "outcomes",
    "start_time",
    "success",
    "timeout",
    "total_mutants",
    "unviable",
}


def load_outcomes(path: Path) -> list[dict[str, object]]:
    metadata = path.lstat()
    if not path.is_file() or path.is_symlink():
        raise ValueError("mutation evidence must be a regular non-symlink file")
    if metadata.st_size > MAX_EVIDENCE_BYTES:
        raise ValueError("mutation evidence exceeds 16 MiB")
    with path.open("rb") as handle:
        payload = handle.read(MAX_EVIDENCE_BYTES + 1)
    if len(payload) > MAX_EVIDENCE_BYTES:
        raise ValueError("mutation evidence grew beyond 16 MiB")
    document = json.loads(payload)
    if not isinstance(document, dict) or set(document) != EVIDENCE_KEYS:
        raise ValueError("mutation evidence has an unexpected top-level schema")
    outcomes = document["outcomes"]
    if not isinstance(outcomes, list) or not outcomes:
        raise ValueError("mutation evidence must contain outcomes")
    if any(not isinstance(outcome, dict) for outcome in outcomes):
        raise ValueError("mutation outcomes must be objects")
    return outcomes


def check_score(
    outcomes: list[dict[str, object]], minimum_score: float
) -> dict[str, object]:
    if not 0.0 <= minimum_score <= 100.0:
        raise ValueError("minimum mutation score must be between 0 and 100")

    baselines = [
        outcome
        for outcome in outcomes
        if outcome.get("scenario") == "Baseline"
    ]
    if len(baselines) != 1 or baselines[0].get("summary") != "Success":
        raise ValueError("mutation baseline did not complete successfully")

    counts = {summary: 0 for summary in MUTANT_SUMMARIES}
    for outcome in outcomes:
        if outcome.get("scenario") == "Baseline":
            continue
        summary = outcome.get("summary")
        if summary not in MUTANT_SUMMARIES:
            raise ValueError(f"unexpected mutation outcome: {summary!r}")
        counts[str(summary)] += 1

    caught = counts["CaughtMutant"]
    missed = counts["MissedMutant"]
    timeouts = counts["Timeout"]
    scored = caught + missed
    if scored == 0:
        raise ValueError("mutation evidence contains no scored mutants")
    if timeouts:
        raise ValueError(f"mutation run contained {timeouts} timed-out mutants")

    score = 100.0 * caught / scored
    result: dict[str, object] = {
        "gate": "mutation_score",
        "status": "pass",
        "score": round(score, 2),
        "minimum_score": minimum_score,
        "caught": caught,
        "missed": missed,
        "unviable": counts["Unviable"],
        "timeouts": timeouts,
    }
    if score < minimum_score:
        raise ValueError(
            f"mutation score {score:.2f}% is below required {minimum_score:.2f}%"
        )
    return result


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("evidence", type=Path)
    parser.add_argument("--minimum-score", type=float, required=True)
    args = parser.parse_args()
    result = check_score(load_outcomes(args.evidence), args.minimum_score)
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
