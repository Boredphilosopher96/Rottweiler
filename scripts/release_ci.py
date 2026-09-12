"""Qualify a release SHA against its exact successful main CI run."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import time
from urllib.parse import urlencode


WORKFLOW = "ci.yml"
WORKFLOW_PATH = ".github/workflows/ci.yml"
BRANCH = "main"
EVENT = "push"
REQUIRED_JOBS = (
    "CI required",
    "M5 Linux sandbox security acceptance",
    "Test (ubuntu-latest)",
    "Test (macos-15)",
    "Supply chain",
    "Build native Linux candidate",
    "Build native macOS candidate",
)
RUN_STATUSES = {"queued", "in_progress", "completed", "requested", "waiting", "pending"}


class ContractError(ValueError):
    """The GitHub result cannot satisfy the trusted CI contract."""


class GitHub:
    def __init__(self, repository: str):
        if re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository) is None:
            raise ValueError("invalid repository")
        self.repository = repository
        self.deadline = time.monotonic() + 120

    def request(self, path: str) -> dict:
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("GitHub operation budget expired")
        command = [
            "gh",
            "api",
            f"repos/{self.repository}/{path}",
            "-H",
            "X-GitHub-Api-Version: 2026-03-10",
        ]
        result = subprocess.run(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=min(30, remaining),
            check=False,
        )
        if result.returncode:
            raise RuntimeError(f"GitHub API request failed with status {result.returncode}")
        value = json.loads(result.stdout)
        if not isinstance(value, dict):
            raise ContractError("GitHub API returned a non-object response")
        return value

    def pages(self, path: str, field: str) -> list[dict]:
        rows: list[dict] = []
        for page in range(1, 101):
            separator = "&" if "?" in path else "?"
            result = self.request(f"{path}{separator}per_page=100&page={page}")
            batch = result.get(field)
            if not isinstance(batch, list) or not all(isinstance(row, dict) for row in batch):
                raise ContractError(f"GitHub API response has invalid {field}")
            rows.extend(batch)
            if len(batch) < 100:
                return rows
        raise ContractError("GitHub result exceeded pagination bound")


def positive_int(value: object, label: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 1:
        raise ContractError(f"CI run has invalid {label}")
    return value


def repository_name(value: object, label: str) -> str:
    if not isinstance(value, dict) or not isinstance(value.get("full_name"), str):
        raise ContractError(f"CI run has invalid {label}")
    return value["full_name"]


def summarize_run(run: dict) -> dict:
    fields = (
        "id", "run_attempt", "run_number", "status", "conclusion", "event",
        "head_branch", "head_sha", "path", "html_url", "created_at", "updated_at",
    )
    summary = {field: run.get(field) for field in fields}
    summary["repository"] = repository_name(run.get("repository"), "repository")
    summary["head_repository"] = repository_name(run.get("head_repository"), "head repository")
    return summary


def validate_run(run: dict, repository: str, source_sha: str) -> None:
    positive_int(run.get("id"), "id")
    positive_int(run.get("run_attempt"), "attempt")
    path = run.get("path")
    status = run.get("status")
    if (
        repository_name(run.get("repository"), "repository") != repository
        or repository_name(run.get("head_repository"), "head repository") != repository
        or run.get("head_branch") != BRANCH
        or run.get("head_sha") != source_sha
        or run.get("event") != EVENT
        or not isinstance(path, str)
        or path.split("@", 1)[0] != WORKFLOW_PATH
        or status not in RUN_STATUSES
    ):
        raise ContractError("CI run is not the expected repository, workflow, event, branch, and SHA")
    conclusion = run.get("conclusion")
    if status == "completed" and not isinstance(conclusion, str):
        raise ContractError("completed CI run has no conclusion")
    if status != "completed" and conclusion is not None:
        raise ContractError("incomplete CI run has a terminal conclusion")


def select_latest_run(runs: list[dict], repository: str, source_sha: str) -> dict | None:
    seen_ids: set[int] = set()
    for run in runs:
        validate_run(run, repository, source_sha)
        run_id = run["id"]
        if run_id in seen_ids:
            raise ContractError("CI query returned a duplicate run id")
        seen_ids.add(run_id)
    return max(runs, key=lambda run: run["id"], default=None)


def summarize_job(job: dict) -> dict:
    fields = (
        "id", "run_id", "name", "run_attempt", "status", "conclusion", "started_at",
        "completed_at", "html_url",
    )
    return {field: job.get(field) for field in fields}


def validate_jobs(jobs: list[dict], run_id: int, attempt: int) -> None:
    if not jobs:
        raise ContractError("completed CI attempt returned no jobs")
    seen_ids: set[int] = set()
    names: dict[str, int] = {}
    for job in jobs:
        job_id = positive_int(job.get("id"), "job id")
        if job_id in seen_ids:
            raise ContractError("CI attempt returned a duplicate job id")
        seen_ids.add(job_id)
        name = job.get("name")
        if not isinstance(name, str) or not name:
            raise ContractError("CI attempt returned a job without a name")
        if positive_int(job.get("run_id"), "job run id") != run_id:
            raise ContractError("CI job does not belong to the selected run")
        if positive_int(job.get("run_attempt"), "job attempt") != attempt:
            raise ContractError("CI job does not belong to the latest run attempt")
        if job.get("status") != "completed" or job.get("conclusion") != "success":
            raise ContractError(f"CI job did not succeed: {name}")
        names[name] = names.get(name, 0) + 1
    missing = [name for name in REQUIRED_JOBS if names.get(name, 0) == 0]
    duplicate = [name for name in REQUIRED_JOBS if names.get(name, 0) > 1]
    if missing:
        raise ContractError("CI attempt is missing required jobs: " + ", ".join(missing))
    if duplicate:
        raise ContractError("CI attempt has duplicate required jobs: " + ", ".join(duplicate))


def write_report(path: Path, report: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(report, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(path)


def write_outputs(path: Path, run_id: int, attempt: int, evidence: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as stream:
        print(f"ci_run_id={run_id}", file=stream)
        print(f"ci_run_attempt={attempt}", file=stream)
        print(f"ci_evidence_path={evidence}", file=stream)


def qualify(
    api: GitHub,
    source_sha: str,
    output: Path,
    github_output: Path,
    *,
    timeout_seconds: int = 3600,
    poll_seconds: int = 30,
    clock=time.monotonic,
    sleep=time.sleep,
) -> int:
    if re.fullmatch(r"[0-9a-f]{40}", source_sha) is None:
        raise ValueError("invalid source SHA")
    if timeout_seconds < 1 or poll_seconds < 1 or poll_seconds > timeout_seconds:
        raise ValueError("invalid polling bounds")
    report = {
        "schema_version": 1,
        "status": "waiting",
        "source_sha": source_sha,
        "expected": {
            "repository": api.repository,
            "workflow": WORKFLOW_PATH,
            "event": EVENT,
            "branch": BRANCH,
        },
        "required_jobs": list(REQUIRED_JOBS),
    }
    write_report(output, report)
    started = clock()
    deadline = started + timeout_seconds
    api.deadline = deadline
    query = urlencode({"branch": BRANCH, "event": EVENT, "head_sha": source_sha})
    try:
        while True:
            if clock() >= deadline:
                raise TimeoutError("exact main CI run did not complete before the deadline")
            runs = api.pages(f"actions/workflows/{WORKFLOW}/runs?{query}", "workflow_runs")
            run = select_latest_run(runs, api.repository, source_sha)
            report["observed_run_ids"] = sorted(run["id"] for run in runs)
            if run is None:
                report["wait_state"] = "run_not_found"
            else:
                report["ci_run"] = summarize_run(run)
                report["wait_state"] = "run_" + run["status"]
                if run["status"] == "completed":
                    if run["conclusion"] != "success":
                        raise ContractError("latest exact main CI run did not succeed")
                    attempt = run["run_attempt"]
                    jobs = api.pages(
                        f"actions/runs/{run['id']}/attempts/{attempt}/jobs?filter=all",
                        "jobs",
                    )
                    report["jobs"] = sorted(
                        (summarize_job(job) for job in jobs),
                        key=lambda job: (str(job["name"]), int(job["id"] or 0)),
                    )
                    validate_jobs(jobs, run["id"], attempt)
                    refreshed = api.request(f"actions/runs/{run['id']}")
                    validate_run(refreshed, api.repository, source_sha)
                    if refreshed["id"] != run["id"]:
                        raise ContractError("CI run detail returned the wrong run id")
                    identity = ("run_attempt", "status", "conclusion")
                    if any(refreshed[field] != run[field] for field in identity):
                        report["ci_run"] = summarize_run(refreshed)
                        report["wait_state"] = "run_changed_during_validation"
                        report["elapsed_seconds"] = round(clock() - started, 3)
                        write_report(output, report)
                        sleep(min(poll_seconds, max(0, deadline - clock())))
                        continue
                    report.update(status="qualified", wait_state="complete", job_count=len(jobs))
                    write_report(output, report)
                    write_outputs(github_output, run["id"], attempt, output)
                    return 0
            report["elapsed_seconds"] = round(clock() - started, 3)
            write_report(output, report)
            sleep(min(poll_seconds, max(0, deadline - clock())))
    except Exception as error:
        report.update(
            status="blocked",
            blocker=str(error),
            error=type(error).__name__,
            elapsed_seconds=round(clock() - started, 3),
        )
        write_report(output, report)
        return 1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--github-output", type=Path, default=os.environ.get("GITHUB_OUTPUT"))
    parser.add_argument("--timeout-seconds", type=int, default=3600)
    parser.add_argument("--poll-seconds", type=int, default=30)
    args = parser.parse_args()
    if args.github_output is None:
        parser.error("--github-output or GITHUB_OUTPUT is required")
    return qualify(
        GitHub(args.repository),
        args.source_sha,
        args.output,
        args.github_output,
        timeout_seconds=args.timeout_seconds,
        poll_seconds=args.poll_seconds,
    )


if __name__ == "__main__":
    raise SystemExit(main())
