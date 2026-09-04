"""Dispatch one verified platform candidate and bound its runner queue wait."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
PLATFORMS = json.loads((ROOT / "contracts/soak-platforms.json").read_text())
WORKER = "protected-soak.yml"
QUEUE_SECONDS = 15 * 60


class GitHub:
    def __init__(self, repository: str):
        if re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository) is None:
            raise ValueError("invalid repository")
        self.repository = repository
        self.deadline = time.monotonic() + 120

    def request(self, path: str, payload: dict | None = None) -> dict:
        remaining = self.deadline - time.monotonic()
        if remaining <= 0 and not path.endswith("/force-cancel"):
            raise TimeoutError("GitHub operation budget expired")
        command = ["gh", "api", f"repos/{self.repository}/{path}",
                   "-H", "X-GitHub-Api-Version: 2026-03-10"]
        if payload is not None:
            command += ["--method", "POST", "--input", "-"]
        result = subprocess.run(command, input=None if payload is None else json.dumps(payload).encode(),
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                timeout=30 if path.endswith("/force-cancel") else min(30, remaining), check=False)
        if result.returncode:
            raise RuntimeError("GitHub API request failed")
        return json.loads(result.stdout) if result.stdout.strip() else {}

    def pages(self, path: str, field: str) -> list[dict]:
        rows = []
        for page in range(1, 101):
            separator = "&" if "?" in path else "?"
            result = self.request(f"{path}{separator}per_page=100&page={page}")
            batch = result[field]
            rows.extend(batch)
            if len(batch) < 100:
                return rows
        raise ValueError("GitHub result exceeded pagination bound")


def artifact_names(platform: str, run_id: int, attempt: int) -> tuple[str, str]:
    prefix = PLATFORMS[platform]["artifact_prefix"]
    return (f"{prefix}-performance-rw-{run_id}-{attempt}",
            f"{prefix}-soak-tui-{run_id}-{attempt}")


def validate_candidate(api: GitHub, platform: str, run_id: int, attempt: int, sha: str) -> tuple[str, str]:
    import yaml
    if run_id < 1 or attempt < 1 or re.fullmatch(r"[0-9a-f]{40}", sha) is None:
        raise ValueError("invalid candidate identity")
    run = api.request(f"actions/runs/{run_id}")
    if (run["head_repository"]["full_name"] != api.repository or run["head_branch"] != "main"
            or run["head_sha"] != sha or run["run_attempt"] != attempt
            or run["path"].split("@")[0] != ".github/workflows/nightly.yml"
            or run["event"] not in ("schedule", "workflow_dispatch")):
        raise ValueError("candidate is not the expected trusted nightly attempt")
    workflow = yaml.load((ROOT / ".github/workflows/nightly.yml").read_text(), Loader=yaml.BaseLoader)
    build_name = workflow["jobs"][PLATFORMS[platform]["build_job"]]["name"]
    jobs = api.pages(f"actions/runs/{run_id}/jobs?filter=all", "jobs")
    matching = [job for job in jobs if job["name"] == build_name and job["run_attempt"] <= attempt]
    producer_attempt = max((job["run_attempt"] for job in matching), default=0)
    matching = [job for job in matching if job["run_attempt"] == producer_attempt]
    if len(matching) != 1 or matching[0]["conclusion"] != "success":
        raise ValueError("platform build did not succeed")
    names = artifact_names(platform, run_id, producer_attempt)
    artifacts = api.pages(f"actions/runs/{run_id}/artifacts", "artifacts")
    for name in names:
        matches = [item for item in artifacts if item["name"] == name]
        if len(matches) != 1 or matches[0]["expired"]:
            raise ValueError("candidate artifact missing, ambiguous, or expired")
    return names


def write_report(path: Path, report: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(report, sort_keys=True) + "\n")
    temporary.replace(path)


def watch_worker(api: GitHub, child_id: int, output: Path, *, clock=time.monotonic,
                 sleep=time.sleep, queue_seconds=QUEUE_SECONDS) -> int:
    report = {"schema_version": 1, "worker_run_id": child_id,
              "status": "queued", "qualification": "not_exercised"}
    write_report(output, report)
    started = clock()
    api.deadline = started + queue_seconds
    try:
        while clock() - started < queue_seconds:
            run = api.request(f"actions/runs/{child_id}")
            jobs = api.pages(f"actions/runs/{child_id}/jobs", "jobs")
            workload = [job for job in jobs if job["name"] == "Eight-hour workload"]
            if len(workload) > 1:
                raise ValueError("ambiguous worker workload")
            if workload and workload[0]["status"] == "in_progress":
                report.update(status="started", qualification="pending",
                              queue_seconds=round(clock() - started, 3))
                write_report(output, report)
                return 0
            if run["status"] == "completed":
                # Even a green workflow without an exercised workload is not qualification.
                report.update(status="worker_ended_before_observed_start", worker_conclusion=run["conclusion"])
                raise ValueError("workload did not start while queue was observed")
            report.update(queue_seconds=round(clock() - started, 3))
            write_report(output, report)
            sleep(min(10, max(0, queue_seconds - (clock() - started))))
        report["status"] = "infrastructure_unavailable"
        raise TimeoutError("runner queue deadline exceeded")
    except (Exception, KeyboardInterrupt) as error:
        report.update(error=type(error).__name__, elapsed_seconds=round(clock() - started, 3))
        if report["status"] == "queued":
            report["status"] = "infrastructure_unavailable"
        # The workflow uploads this result before cancelling itself. Cancelling
        # here would prevent even always-run artifact steps from executing.
        report["cancel_required"] = True
        write_report(output, report)
        return 1


def dispatch(api: GitHub, platform: str, run_id: int, attempt: int, sha: str, output: Path,
             *, clock=time.monotonic) -> int:
    report = {"schema_version": 1, "platform": platform, "source_sha": sha,
              "candidate_run_id": run_id, "candidate_attempt": attempt,
              "status": "validating", "qualification": "not_exercised"}
    write_report(output, report)
    child_id = None
    dispatch_attempted = False
    correlation = uuid.uuid4().hex
    title = f"Soak {platform} / {run_id}.{attempt} / {sha} / {correlation}"
    report["correlation_id"] = correlation
    write_report(output, report)
    started = clock()
    try:
        validate_candidate(api, platform, run_id, attempt, sha)
        report["status"] = "dispatching"
        write_report(output, report)
        dispatch_attempted = True
        response = api.request(f"actions/workflows/{WORKER}/dispatches", {
            "ref": "main", "inputs": {"platform": platform, "candidate_run_id": str(run_id),
            "candidate_attempt": str(attempt), "candidate_sha": sha, "correlation_id": correlation},
        })
        child_id = response.get("workflow_run_id")
        if not isinstance(child_id, int) or isinstance(child_id, bool) or child_id < 1:
            raise ValueError("dispatch did not return an owned workflow run id")
        report.update(worker_run_id=child_id, status="queued")
        write_report(output, report)
        report.update(status="dispatched", qualification="pending")
        write_report(output, report)
        return 0
    except (Exception, KeyboardInterrupt) as error:
        report.update(error=type(error).__name__, elapsed_seconds=round(clock() - started, 3))
        if report["status"] in ("validating", "dispatching", "queued"):
            report["status"] = "infrastructure_unavailable"
        if child_id is None and dispatch_attempted:
            report["dispatch_outcome_unknown"] = True
        if child_id is None and dispatch_attempted:
            # A dispatch may have been accepted before the API connection was
            # lost. Recover only an exact unguessable correlation, never a
            # nearby run or another platform's worker.
            try:
                runs = api.pages(f"actions/workflows/{WORKER}/runs", "workflow_runs")
                matching = [run for run in runs if run.get("display_title") == title]
                if len(matching) == 1:
                    child_id = matching[0]["id"]
                    report["worker_run_id"] = child_id
            except Exception:
                report["correlation_lookup_unavailable"] = True
        if child_id is not None:
            try:
                api.request(f"actions/runs/{child_id}/force-cancel", {})
                report["worker_cancel_requested"] = True
            except Exception as cancel_error:
                report["worker_cancel_error"] = type(cancel_error).__name__
        write_report(output, report)
        return 1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["dispatch", "validate", "watch"])
    parser.add_argument("--repository", required=True)
    parser.add_argument("--platform", choices=sorted(PLATFORMS), required=True)
    parser.add_argument("--run-id", type=int, required=True)
    parser.add_argument("--attempt", type=int, required=True)
    parser.add_argument("--sha", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    api = GitHub(args.repository)
    if args.command == "validate":
        names = validate_candidate(api, args.platform, args.run_id, args.attempt, args.sha)
        result = {"engine_artifact": names[0], "tui_artifact": names[1],
                  "runner": json.dumps(PLATFORMS[args.platform]["runner_labels"])}
        write_report(args.output, result)
        with open(os.environ["GITHUB_OUTPUT"], "a") as stream:
            for name, value in result.items():
                print(f"{name}={value}", file=stream)
        return 0
    def interrupted(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    if args.command == "watch":
        # The token belongs to this worker workflow, so cancellation cannot be
        # redirected through a caller-supplied run id.
        return watch_worker(api, int(os.environ["GITHUB_RUN_ID"]), args.output)
    return dispatch(api, args.platform, args.run_id, args.attempt, args.sha, args.output)


if __name__ == "__main__":
    raise SystemExit(main())
