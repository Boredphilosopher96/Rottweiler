#!/usr/bin/env python3
"""Validate retained Harbor evidence against an approved benchmark baseline."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import re
import stat
from pathlib import Path


MAX_FILE_BYTES = 1024 * 1024
EXPECTED_DATASET = "terminal-bench/terminal-bench-2-1@6"
TASK_NAME = re.compile(r"terminal-bench/[a-z0-9-][a-z0-9-]*")
DEFAULT_TASK_LIST = Path(__file__).resolve().parents[1] / "evals" / "terminal-bench-20.txt"


def load_json(path: Path) -> dict[str, object]:
    metadata = path.lstat()
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        raise ValueError(f"{path} is not a regular evidence file")
    if metadata.st_nlink != 1 or metadata.st_size > MAX_FILE_BYTES:
        raise ValueError(f"{path} has unsafe links or size")
    with path.open("rb") as handle:
        data = handle.read(MAX_FILE_BYTES + 1)
        after = os.fstat(handle.fileno())
    if len(data) > MAX_FILE_BYTES or (metadata.st_dev, metadata.st_ino, metadata.st_size) != (
        after.st_dev,
        after.st_ino,
        after.st_size,
    ):
        raise ValueError(f"{path} changed while it was read")
    value = json.loads(data)
    if not isinstance(value, dict):
        raise ValueError(f"{path} must contain a JSON object")
    return value


def load_task_list(path: Path) -> set[str]:
    metadata = path.lstat()
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        raise ValueError("Terminal-Bench task list must be a regular file")
    if metadata.st_nlink != 1 or metadata.st_size > MAX_FILE_BYTES:
        raise ValueError("Terminal-Bench task list has unsafe links or size")
    tasks = path.read_text(encoding="utf-8").splitlines()
    if (
        len(tasks) != 20
        or len(tasks) != len(set(tasks))
        or any(TASK_NAME.fullmatch(task) is None for task in tasks)
    ):
        raise ValueError("Terminal-Bench task list must contain 20 unique canonical names")
    return set(tasks)


def primary_reward(result: dict[str, object]) -> float:
    verifier = result.get("verifier_result")
    if not isinstance(verifier, dict):
        raise ValueError("trial has no verifier result")
    rewards = verifier.get("rewards")
    if not isinstance(rewards, dict) or "reward" not in rewards:
        raise ValueError("trial has no primary reward")
    reward = rewards["reward"]
    if not isinstance(reward, (int, float)) or isinstance(reward, bool) or not 0 <= reward <= 1:
        raise ValueError("trial reward must be numeric in [0, 1]")
    return float(reward)


def duration_seconds(result: dict[str, object]) -> float:
    try:
        start = dt.datetime.fromisoformat(str(result["started_at"]).replace("Z", "+00:00"))
        finish = dt.datetime.fromisoformat(str(result["finished_at"]).replace("Z", "+00:00"))
    except (KeyError, ValueError) as error:
        raise ValueError("trial timing is missing or malformed") from error
    elapsed = (finish - start).total_seconds()
    if elapsed < 0 or elapsed > 24 * 60 * 60:
        raise ValueError("trial duration is outside the accepted bound")
    return elapsed


def stats_metrics(stats: dict[str, object]) -> tuple[int, int]:
    usage = stats.get("usage")
    cost = stats.get("cost")
    if not isinstance(usage, dict) or not isinstance(cost, dict):
        raise ValueError("Rottweiler stats are incomplete")
    inputs = usage.get("input_tokens")
    outputs = usage.get("output_tokens")
    micros = cost.get("known_usd_micros")
    complete = cost.get("usd_cost_complete")
    if any(not isinstance(value, int) or isinstance(value, bool) or value < 0 for value in (inputs, outputs, micros)):
        raise ValueError("Rottweiler token or cost stats are malformed")
    if complete is not True:
        raise ValueError("benchmark cost attribution is incomplete")
    return inputs + outputs, micros


def evaluate(
    root: Path,
    baseline: dict[str, object],
    task_list: Path = DEFAULT_TASK_LIST,
    expected_git: str | None = None,
    archive: Path | None = None,
) -> dict[str, object]:
    manifest = load_json(root / "rottweiler-eval-manifest.json")
    required_baseline = {
        "dataset",
        "model",
        "task_count",
        "minimum_solve_rate",
        "maximum_mean_tokens",
        "maximum_mean_wall_seconds",
        "maximum_mean_cost_usd_micros",
    }
    if set(baseline) != required_baseline:
        raise ValueError("Terminal-Bench baseline schema is invalid")
    if manifest.get("dataset") != EXPECTED_DATASET or baseline["dataset"] != EXPECTED_DATASET:
        raise ValueError("Terminal-Bench dataset does not match the pinned baseline")
    if manifest.get("model") != baseline["model"]:
        raise ValueError("Terminal-Bench model does not match the pinned baseline")
    if manifest.get("harbor_version") != "0.18.0":
        raise ValueError("Terminal-Bench evidence used the wrong Harbor version")
    git_commit = manifest.get("git_commit")
    archive_sha256 = manifest.get("release_archive_sha256")
    if not isinstance(git_commit, str) or re.fullmatch(r"[0-9a-f]{40}", git_commit) is None:
        raise ValueError("Terminal-Bench evidence has no exact Git commit")
    if expected_git is not None and git_commit != expected_git:
        raise ValueError("Terminal-Bench evidence belongs to another Git commit")
    if not isinstance(archive_sha256, str) or re.fullmatch(r"[0-9a-f]{64}", archive_sha256) is None:
        raise ValueError("Terminal-Bench evidence has no exact archive digest")
    if archive is not None:
        digest = hashlib.sha256()
        with archive.open("rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(chunk)
        if digest.hexdigest() != archive_sha256:
            raise ValueError("Terminal-Bench evidence belongs to another release archive")
    task_count = baseline["task_count"]
    expected_tasks = load_task_list(task_list)
    if task_count != len(expected_tasks) or manifest.get("task_count") != task_count:
        raise ValueError("Terminal-Bench must contain the exact 20-task subset")
    trial_paths = []
    for path in sorted(root.rglob("result.json")):
        if path.parent == root or not (path.parent / "agent" / "rottweiler-stats.json").exists():
            continue
        trial_paths.append(path)
    if len(trial_paths) != task_count:
        raise ValueError(f"expected {task_count} completed trials, found {len(trial_paths)}")
    rewards: list[float] = []
    tokens: list[int] = []
    costs: list[int] = []
    durations: list[float] = []
    tasks: set[str] = set()
    for path in trial_paths:
        result = load_json(path)
        if result.get("exception_info") is not None:
            raise ValueError("Terminal-Bench trial contains an exception")
        task = result.get("task_name")
        if not isinstance(task, str) or task not in expected_tasks or task in tasks:
            raise ValueError("Terminal-Bench task is outside the checked-in subset or duplicated")
        tasks.add(task)
        rewards.append(primary_reward(result))
        durations.append(duration_seconds(result))
        token_count, cost = stats_metrics(load_json(path.parent / "agent" / "rottweiler-stats.json"))
        tokens.append(token_count)
        costs.append(cost)
    if tasks != expected_tasks:
        raise ValueError("Terminal-Bench evidence does not match the checked-in task subset")
    observed = {
        "solve_rate": sum(rewards) / task_count,
        "mean_tokens": sum(tokens) / task_count,
        "mean_wall_seconds": sum(durations) / task_count,
        "mean_cost_usd_micros": sum(costs) / task_count,
    }
    comparisons = (
        ("solve_rate", "minimum_solve_rate", lambda value, limit: value >= limit),
        ("mean_tokens", "maximum_mean_tokens", lambda value, limit: value <= limit),
        ("mean_wall_seconds", "maximum_mean_wall_seconds", lambda value, limit: value <= limit),
        ("mean_cost_usd_micros", "maximum_mean_cost_usd_micros", lambda value, limit: value <= limit),
    )
    for metric, threshold, predicate in comparisons:
        limit = baseline[threshold]
        if not isinstance(limit, (int, float)) or isinstance(limit, bool) or limit < 0:
            raise ValueError(f"baseline {threshold} is invalid")
        if not predicate(observed[metric], limit):
            raise ValueError(f"Terminal-Bench {metric} regressed: {observed[metric]} vs {limit}")
    return {"status": "pass", "dataset": EXPECTED_DATASET, "model": baseline["model"], **observed}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("evidence", type=Path)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("--expected-git")
    parser.add_argument("--archive", type=Path)
    args = parser.parse_args()
    print(
        json.dumps(
            evaluate(
                args.evidence,
                load_json(args.baseline),
                expected_git=args.expected_git,
                archive=args.archive,
            ),
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
