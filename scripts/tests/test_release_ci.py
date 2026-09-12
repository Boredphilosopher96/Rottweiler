import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


SPEC = importlib.util.spec_from_file_location(
    "release_ci", Path(__file__).resolve().parents[1] / "release_ci.py"
)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
SHA = "a" * 40


def run_record(run_id=100, *, status="completed", conclusion="success", attempt=2):
    return {
        "id": run_id,
        "run_attempt": attempt,
        "run_number": 50,
        "status": status,
        "conclusion": conclusion if status == "completed" else None,
        "event": "push",
        "head_branch": "main",
        "head_sha": SHA,
        "path": ".github/workflows/ci.yml@refs/heads/main",
        "repository": {"full_name": "owner/repo"},
        "head_repository": {"full_name": "owner/repo"},
        "html_url": "https://example.invalid/run/100",
        "created_at": "2026-09-12T00:00:00Z",
        "updated_at": "2026-09-12T00:01:00Z",
    }


def successful_jobs(attempt=2):
    names = list(MODULE.REQUIRED_JOBS) + ["Additional contract check"]
    return [
        {
            "id": index,
            "run_id": 100,
            "name": name,
            "run_attempt": attempt,
            "status": "completed",
            "conclusion": "success",
            "started_at": "2026-09-12T00:00:00Z",
            "completed_at": "2026-09-12T00:01:00Z",
            "html_url": f"https://example.invalid/job/{index}",
        }
        for index, name in enumerate(names, start=1)
    ]


class FakeGitHub:
    repository = "owner/repo"

    def __init__(self, run_pages, jobs=None, details=None):
        self.run_pages = list(run_pages)
        self.job_pages = [successful_jobs() if jobs is None else jobs]
        self.details = list(details or [run_record()])
        self.calls = []
        self.deadline = None

    def request(self, path):
        self.calls.append((path, None))
        if path.startswith("actions/runs/"):
            return self.details.pop(0)
        raise AssertionError(path)

    def pages(self, path, field):
        self.calls.append((path, field))
        if field == "workflow_runs":
            page = self.run_pages.pop(0)
            return page
        if field == "jobs":
            return self.job_pages.pop(0)
        raise AssertionError((path, field))


class ReleaseCiTests(unittest.TestCase):
    def execute(self, api, *, timeout=90, poll=30):
        now = [0.0]
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        evidence = root / "release-ci.json"
        github_output = root / "github-output"
        code = MODULE.qualify(
            api,
            SHA,
            evidence,
            github_output,
            timeout_seconds=timeout,
            poll_seconds=poll,
            clock=lambda: now[0],
            sleep=lambda seconds: now.__setitem__(0, now[0] + seconds),
        )
        output = github_output.read_text() if github_output.exists() else ""
        return code, json.loads(evidence.read_text()), output

    def test_waits_for_exact_run_then_emits_latest_attempt_receipt(self):
        api = FakeGitHub([
            [],
            [run_record(status="in_progress")],
            [run_record()],
        ])
        code, report, output = self.execute(api)

        self.assertEqual(code, 0)
        self.assertEqual(report["status"], "qualified")
        self.assertEqual(report["ci_run"]["head_sha"], SHA)
        self.assertEqual(report["job_count"], len(successful_jobs()))
        self.assertIn("ci_run_id=100\n", output)
        self.assertIn("ci_run_attempt=2\n", output)
        self.assertIn("ci_evidence_path=", output)
        self.assertIn("actions/runs/100/attempts/2/jobs?filter=all", api.calls[-2][0])
        self.assertEqual(api.calls[-1][0], "actions/runs/100")
        self.assertTrue(all(job["run_id"] == 100 for job in report["jobs"]))

    def test_run_metadata_must_match_every_trust_boundary(self):
        mutations = {
            "head_sha": "b" * 40,
            "head_branch": "feature",
            "event": "workflow_dispatch",
            "path": ".github/workflows/release.yml",
            "repository": {"full_name": "other/repo"},
            "head_repository": {"full_name": "other/repo"},
        }
        for field, value in mutations.items():
            with self.subTest(field=field):
                run = run_record()
                run[field] = value
                code, report, output = self.execute(FakeGitHub([[run]]))
                self.assertEqual(code, 1)
                self.assertEqual(report["status"], "blocked")
                self.assertEqual(output, "")

    def test_latest_run_cannot_fall_back_to_older_success(self):
        failed = run_record(101, conclusion="failure")
        code, report, _ = self.execute(FakeGitHub([[run_record(100), failed]]))

        self.assertEqual(code, 1)
        self.assertEqual(report["ci_run"]["id"], 101)
        self.assertIn("did not succeed", report["blocker"])

    def test_every_job_must_be_latest_attempt_success_and_required(self):
        cases = []
        failed = successful_jobs()
        failed[-1]["conclusion"] = "skipped"
        cases.append(("non-required skipped", failed, "did not succeed"))
        stale = successful_jobs()
        stale[0]["run_attempt"] = 1
        cases.append(("stale attempt", stale, "latest run attempt"))
        foreign = successful_jobs()
        foreign[0]["run_id"] = 101
        cases.append(("foreign run", foreign, "selected run"))
        missing = successful_jobs()
        missing = [job for job in missing if job["name"] != MODULE.REQUIRED_JOBS[0]]
        cases.append(("missing required", missing, "missing required jobs"))
        for label, jobs, blocker in cases:
            with self.subTest(label=label):
                code, report, output = self.execute(FakeGitHub([[run_record()]], jobs))
                self.assertEqual(code, 1)
                self.assertIn(blocker, report["blocker"])
                self.assertEqual(output, "")

    def test_attempt_change_during_job_validation_restarts_selection(self):
        first_jobs = successful_jobs(attempt=2)
        second_jobs = successful_jobs(attempt=3)
        api = FakeGitHub(
            [[run_record(attempt=2)], [run_record(attempt=3)]],
            first_jobs,
            details=[run_record(status="in_progress", attempt=3), run_record(attempt=3)],
        )
        api.job_pages.append(second_jobs)
        code, report, output = self.execute(api)

        self.assertEqual(code, 0)
        self.assertEqual(report["ci_run"]["run_attempt"], 3)
        self.assertIn("ci_run_attempt=3\n", output)

    def test_missing_run_times_out_with_retained_evidence(self):
        code, report, output = self.execute(FakeGitHub([[], []]), timeout=30, poll=30)

        self.assertEqual(code, 1)
        self.assertEqual(report["status"], "blocked")
        self.assertEqual(report["error"], "TimeoutError")
        self.assertEqual(report["observed_run_ids"], [])
        self.assertEqual(output, "")


if __name__ == "__main__":
    unittest.main()
