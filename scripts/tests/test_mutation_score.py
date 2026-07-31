import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "check_mutation_score", ROOT / "scripts/check-mutation-score.py"
)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def outcome(summary: str) -> dict[str, object]:
    return {"scenario": {"Mutant": {"name": summary}}, "summary": summary}


class MutationScoreTests(unittest.TestCase):
    def test_accepts_a_successful_baseline_at_the_score_floor(self) -> None:
        outcomes = [
            {"scenario": "Baseline", "summary": "Success"},
            outcome("CaughtMutant"),
            outcome("CaughtMutant"),
            outcome("MissedMutant"),
            outcome("Unviable"),
        ]
        result = MODULE.check_score(outcomes, 60.0)
        self.assertEqual(result["caught"], 2)
        self.assertEqual(result["missed"], 1)
        self.assertEqual(result["score"], 66.67)

    def test_rejects_a_score_below_the_floor(self) -> None:
        outcomes = [
            {"scenario": "Baseline", "summary": "Success"},
            outcome("CaughtMutant"),
            outcome("MissedMutant"),
        ]
        with self.assertRaisesRegex(ValueError, "below required"):
            MODULE.check_score(outcomes, 51.0)

    def test_rejects_timeouts_and_failed_baselines(self) -> None:
        with self.assertRaisesRegex(ValueError, "timed-out"):
            MODULE.check_score(
                [
                    {"scenario": "Baseline", "summary": "Success"},
                    outcome("CaughtMutant"),
                    outcome("Timeout"),
                ],
                0.0,
            )
        with self.assertRaisesRegex(ValueError, "baseline"):
            MODULE.check_score(
                [
                    {"scenario": "Baseline", "summary": "Failure"},
                    outcome("CaughtMutant"),
                ],
                0.0,
            )

    def test_load_outcomes_rejects_unexpected_schema(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            evidence = Path(directory) / "outcomes.json"
            evidence.write_text(json.dumps({"wrong": []}), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "top-level schema"):
                MODULE.load_outcomes(evidence)


if __name__ == "__main__":
    unittest.main()
