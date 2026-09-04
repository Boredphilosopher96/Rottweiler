import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("soak_dispatch", Path(__file__).resolve().parents[1] / "soak_dispatch.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
SHA = "a" * 40


class FakeGitHub:
    repository = "owner/repo"

    def __init__(self, worker="queued"):
        self.worker = worker
        self.calls = []
        self.inputs = None
        self.uncertain = False
        self.expired = False
        self.producer_attempt = 2
        self.producer_conclusion = "success"
        self.run = {"head_repository": {"full_name": self.repository}, "head_branch": "main",
                    "head_sha": SHA, "run_attempt": 2, "path": ".github/workflows/nightly.yml",
                    "event": "schedule"}

    def request(self, path, payload=None):
        self.calls.append((path, payload))
        if path == "actions/runs/100":
            return self.run
        if path.endswith("/dispatches"):
            self.inputs = payload["inputs"]
            if self.uncertain:
                raise RuntimeError("connection lost after acceptance")
            return {"workflow_run_id": 900}
        if path == "actions/runs/900":
            return {"status": "completed" if self.worker == "skipped" else "in_progress",
                    "conclusion": "success" if self.worker == "skipped" else None}
        if path == "actions/runs/900/force-cancel":
            return {}
        raise AssertionError(path)

    def pages(self, path, field):
        if path == "actions/runs/100/jobs?filter=all":
            # Only Linux succeeded. macOS's failure must not suppress it.
            return [{"name": "Build isolated Linux performance binary", "conclusion": self.producer_conclusion,
                     "run_attempt": self.producer_attempt},
                    {"name": "Build isolated macOS performance binary", "conclusion": "failure", "run_attempt": 2}]
        if path == "actions/runs/100/artifacts":
            return [{"name": name, "expired": self.expired}
                    for name in MODULE.artifact_names("linux-x86_64", 100, self.producer_attempt)]
        if path == "actions/runs/900/jobs":
            return [{"name": "Eight-hour workload", "status": self.worker}]
        if path == "actions/workflows/protected-soak.yml/runs":
            title = f"Soak linux-x86_64 / 100.2 / {SHA} / {self.inputs['correlation_id']}"
            return [{"id": 900, "display_title": title}, {"id": 901, "display_title": title + "-other"}]
        raise AssertionError((path, field))


class SoakDispatchTests(unittest.TestCase):
    def execute(self, api, watch=False):
        now = [0.0]
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "queue.json"
            if watch:
                code = MODULE.watch_worker(api, 900, output, clock=lambda: now[0],
                                           sleep=lambda seconds: now.__setitem__(0, now[0] + seconds), queue_seconds=30)
            else:
                code = MODULE.dispatch(api, "linux-x86_64", 100, 2, SHA, output, clock=lambda: now[0])
            return code, json.loads(output.read_text())

    def test_started_is_handoff_not_qualification_and_other_platform_is_independent(self):
        api = FakeGitHub("in_progress")
        code, report = self.execute(api)
        self.assertEqual(code, 0)
        self.assertEqual(report["status"], "dispatched")
        self.assertEqual(report["qualification"], "pending")
        self.assertEqual(report["worker_run_id"], 900)
        self.assertFalse(any("cancel" in path for path, _ in api.calls))

    def test_queue_expiry_records_owned_cancellation_before_artifact_upload(self):
        api = FakeGitHub()
        code, report = self.execute(api, watch=True)
        self.assertEqual(code, 1)
        self.assertEqual(report["status"], "infrastructure_unavailable")
        self.assertEqual(report["qualification"], "not_exercised")
        self.assertEqual(report["elapsed_seconds"], 30)
        self.assertTrue(report["cancel_required"])
        self.assertFalse(any("cancel" in path for path, _ in api.calls))

    def test_green_workflow_with_skipped_workload_does_not_qualify(self):
        code, report = self.execute(FakeGitHub("skipped"), watch=True)
        self.assertEqual(code, 1)
        self.assertEqual(report["qualification"], "not_exercised")

    def test_untrusted_stale_failed_or_expired_candidate_never_dispatches(self):
        for field, value in [("head_sha", "b" * 40), ("head_branch", "feature"),
                             ("run_attempt", 3), ("event", "pull_request"),
                             ("path", ".github/workflows/ci.yml")]:
            with self.subTest(field=field):
                api = FakeGitHub()
                api.run[field] = value
                self.assertEqual(self.execute(api)[0], 1)
                self.assertIsNone(api.inputs)
        api = FakeGitHub()
        api.expired = True
        self.assertEqual(self.execute(api)[0], 1)
        self.assertIsNone(api.inputs)
        with self.assertRaises(ValueError):
            MODULE.validate_candidate(FakeGitHub(), "darwin-arm64", 100, 2, SHA)

    def test_uncertain_dispatch_recovers_only_exact_correlation(self):
        api = FakeGitHub()
        api.uncertain = True
        code, report = self.execute(api)
        self.assertEqual(code, 1)
        self.assertEqual(report["worker_run_id"], 900)
        self.assertTrue(report["worker_cancel_requested"])
        self.assertNotIn("actions/runs/901/force-cancel", [path for path, _ in api.calls])

    def test_failed_job_rerun_reuses_successful_producer_but_never_ignores_new_failure(self):
        api = FakeGitHub("in_progress")
        api.producer_attempt = 1
        names = MODULE.validate_candidate(api, "linux-x86_64", 100, 2, SHA)
        self.assertEqual(names, ("linux-performance-rw-100-1", "linux-soak-tui-100-1"))
        api.producer_attempt = 2
        api.producer_conclusion = "failure"
        self.assertEqual(self.execute(api)[0], 1)
        self.assertIsNone(api.inputs)

    def test_worker_watch_hands_off_only_after_native_workload_is_running(self):
        code, report = self.execute(FakeGitHub("in_progress"), watch=True)
        self.assertEqual(code, 0)
        self.assertEqual(report["status"], "started")
        self.assertEqual(report["qualification"], "pending")
        self.assertNotIn("cancel_required", report)


class QueueOwnershipTests(unittest.TestCase):
    def test_pagination_shares_one_deadline_and_never_launches_after_expiry(self):
        from unittest.mock import patch
        import subprocess
        now = [0.0]
        timeouts = []

        def page(command, **kwargs):
            timeouts.append(kwargs["timeout"])
            now[0] += min(3, kwargs["timeout"])
            return subprocess.CompletedProcess(command, 0, json.dumps({"jobs": [{}] * 100}).encode(), b"")

        with patch.object(MODULE.time, "monotonic", side_effect=lambda: now[0]), \
                patch.object(MODULE.subprocess, "run", side_effect=page) as launched:
            api = MODULE.GitHub("owner/repo")
            api.deadline = 5
            with self.assertRaises(TimeoutError):
                api.pages("actions/runs/900/jobs", "jobs")
            self.assertEqual(timeouts, [5, 2])
            self.assertEqual(launched.call_count, 2)
            self.assertEqual(now[0], 5)

    def test_cancel_has_a_separate_budget_after_queue_deadline(self):
        from unittest.mock import patch
        import subprocess
        with patch.object(MODULE.time, "monotonic", return_value=100), \
                patch.object(MODULE.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, b"", b"")) as launched:
            api = MODULE.GitHub("owner/repo")
            api.deadline = 99
            with self.assertRaises(TimeoutError):
                api.request("actions/runs/900")
            launched.assert_not_called()
            api.request("actions/runs/900/force-cancel", {})
            self.assertEqual(launched.call_args.kwargs["timeout"], 30)

    def test_watch_uses_its_own_run_even_when_candidate_id_differs(self):
        from unittest.mock import patch
        argv = ["soak_dispatch.py", "watch", "--repository", "owner/repo", "--platform", "linux-x86_64",
                "--run-id", "100", "--attempt", "2", "--sha", SHA, "--output", "queue.json"]
        api = FakeGitHub()
        with patch("sys.argv", argv), patch.dict(MODULE.os.environ, {"GITHUB_RUN_ID": "900"}), \
                patch.object(MODULE, "GitHub", return_value=api), \
                patch.object(MODULE.signal, "signal"), \
                patch.object(MODULE, "watch_worker", return_value=0) as watch:
            self.assertEqual(MODULE.main(), 0)
        watch.assert_called_once_with(api, 900, Path("queue.json"))

    def test_bootstrap_failure_still_cancels_only_owned_worker_after_artifact_step(self):
        import os
        import subprocess
        import yaml
        workflow = yaml.load((MODULE.ROOT / ".github/workflows/protected-soak.yml").read_text(), Loader=yaml.BaseLoader)
        job = workflow["jobs"]["queue-watch"]
        steps = job["steps"]
        cleanup = [step for step in steps if "/force-cancel" in step.get("run", "")]
        self.assertEqual(len(cleanup), 1)
        cleanup = cleanup[0]
        # With checkout failed, watch is skipped: cleanup must still be eligible.
        self.assertEqual(cleanup["if"], "${{ always() && steps.watch.outcome != 'success' }}")
        upload_index = next(index for index, step in enumerate(steps) if step.get("uses", "").startswith("actions/upload-artifact@"))
        self.assertGreater(steps.index(cleanup), upload_index)
        self.assertEqual(job["permissions"]["actions"], "write")
        self.assertTrue(all("timeout-minutes" in step for step in steps))
        self.assertLess(sum(int(step["timeout-minutes"]) for step in steps), int(job["timeout-minutes"]))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            capture = root / "arguments"
            gh = root / "gh"
            gh.write_text('#!/bin/sh\nprintf "%s\\n" "$@" > "$CI_TEST_GH_ARGUMENTS"\n')
            gh.chmod(0o700)
            env = dict(os.environ, PATH=f"{root}{os.pathsep}{os.environ.get('PATH', '')}",
                       GITHUB_REPOSITORY="owner/repo", GITHUB_RUN_ID="900", CANDIDATE_RUN="100",
                       CI_TEST_GH_ARGUMENTS=str(capture))
            subprocess.run(["bash", "-e", "-c", cleanup["run"]], env=env, check=True, timeout=3)
            self.assertEqual(capture.read_text().splitlines(),
                             ["api", "--method", "POST", "repos/owner/repo/actions/runs/900/force-cancel"])
        self.assertEqual(set(workflow["jobs"]["qualified"]["needs"]), {"validate", "queue-watch", "soak"})
