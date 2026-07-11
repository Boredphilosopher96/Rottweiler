from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "check-terminal-bench.py"
SPEC = importlib.util.spec_from_file_location("terminal_bench_gate", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class TerminalBenchGateTests(unittest.TestCase):
    def baseline(self) -> dict[str, object]:
        return {
            "dataset": MODULE.EXPECTED_DATASET,
            "model": "openai/gpt-fixture-2026-07-10",
            "task_count": 20,
            "minimum_solve_rate": 0.5,
            "maximum_mean_tokens": 1000,
            "maximum_mean_wall_seconds": 30,
            "maximum_mean_cost_usd_micros": 1000,
        }

    def evidence(self, root: Path, reward: float = 1.0) -> None:
        (root / "rottweiler-eval-manifest.json").write_text(
            json.dumps(
                {
                    "dataset": MODULE.EXPECTED_DATASET,
                    "git_commit": "a" * 40,
                    "harbor_version": "0.18.0",
                    "model": "openai/gpt-fixture-2026-07-10",
                    "release_archive_sha256": "b" * 64,
                    "task_count": 20,
                }
            )
        )
        tasks = sorted(MODULE.load_task_list(MODULE.DEFAULT_TASK_LIST))
        for index, task_name in enumerate(tasks):
            trial = root / "job" / f"trial-{index}"
            (trial / "agent").mkdir(parents=True)
            (trial / "result.json").write_text(
                json.dumps(
                    {
                        "task_name": task_name,
                        "started_at": "2026-07-10T00:00:00Z",
                        "finished_at": "2026-07-10T00:00:10Z",
                        "exception_info": None,
                        "verifier_result": {"rewards": {"reward": reward}},
                    }
                )
            )
            (trial / "agent" / "rottweiler-stats.json").write_text(
                json.dumps(
                    {
                        "usage": {"input_tokens": 600, "output_tokens": 200},
                        "cost": {"known_usd_micros": 500, "usd_cost_complete": True},
                    }
                )
            )

    def test_exact_evidence_passes_and_reports_all_metrics(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.evidence(root)
            report = MODULE.evaluate(root, self.baseline())
            self.assertEqual(report["status"], "pass")
            self.assertEqual(report["solve_rate"], 1.0)
            self.assertEqual(report["mean_tokens"], 800)

    def test_unrelated_twenty_task_run_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.evidence(root)
            result = root / "job" / "trial-0" / "result.json"
            document = json.loads(result.read_text())
            document["task_name"] = "terminal-bench/not-in-the-pinned-subset"
            result.write_text(json.dumps(document))
            with self.assertRaisesRegex(ValueError, "outside the checked-in subset"):
                MODULE.evaluate(root, self.baseline())

    def test_regression_missing_trial_or_incomplete_cost_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.evidence(root, reward=0.0)
            with self.assertRaisesRegex(ValueError, "solve_rate regressed"):
                MODULE.evaluate(root, self.baseline())
            (root / "job" / "trial-0" / "result.json").unlink()
            with self.assertRaisesRegex(ValueError, "completed trials"):
                MODULE.evaluate(root, self.baseline())


if __name__ == "__main__":
    unittest.main()
