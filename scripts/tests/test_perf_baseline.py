from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "check-perf-baseline.py"
SPEC = importlib.util.spec_from_file_location("perf_baseline", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def baseline(value: int = 100) -> dict[str, object]:
    return {
        "schema_version": 1,
        "maximum_regression_fraction": 0.1,
        "platforms": {
            "linux-x86_64": {
                "suites": {
                    "core": {
                        "baseline_kind": "bootstrap",
                        "provenance": "reviewed fixture baseline",
                        "metrics": {"latency_us": value},
                    },
                    "soak": {
                        "baseline_kind": "bootstrap",
                        "provenance": "reviewed fixture baseline",
                        "metrics": {"max_rss_bytes": value},
                    },
                }
            }
        },
    }


def measurement(value: int) -> dict[str, object]:
    return {"schema_version": 1, "metrics": {"latency_us": value}}


def event(*, label: bool, justification: str) -> dict[str, object]:
    return {
        "pull_request": {
            "labels": [{"name": "perf-waiver"}] if label else [],
            "body": f"## Perf waiver justification\n{justification}\n\n## Checklist\n- reviewed",
        }
    }


class PerfBaselineTests(unittest.TestCase):
    def test_checked_in_baseline_has_both_platforms_and_explicit_provenance(self) -> None:
        document = MODULE.load(Path(__file__).parents[2] / "benchmarks/performance-baseline.json")
        self.assertEqual(set(document["platforms"]), {"darwin-arm64", "linux-x86_64"})
        for platform in document["platforms"].values():
            for suite in platform["suites"].values():
                self.assertIn("bootstrap", suite["provenance"])
                self.assertEqual(suite["baseline_kind"], "bootstrap")

    def test_require_measured_rejects_bootstrap_and_accepts_reviewed_measurement(self) -> None:
        document = baseline()
        with self.assertRaisesRegex(ValueError, "bootstrap-only"):
            MODULE.evaluate(
                [measurement(100)],
                document,
                "linux-x86_64",
                "core",
                event=event(label=True, justification="reviewed evidence " * 12),
                require_measured=True,
            )
        document["platforms"]["linux-x86_64"]["suites"]["core"][
            "baseline_kind"
        ] = "measured"
        document["platforms"]["linux-x86_64"]["suites"]["core"][
            "provenance"
        ] = "reviewed workflow run 123 on pinned linux-x86_64 runner"
        result = MODULE.evaluate(
            [measurement(100)],
            document,
            "linux-x86_64",
            "core",
            require_measured=True,
        )
        self.assertEqual(result["status"], "pass")
        self.assertEqual(result["baseline_kind"], "measured")

    def test_accepts_exact_ten_percent_and_rejects_above_it(self) -> None:
        self.assertEqual(
            MODULE.evaluate(
                [measurement(110)], baseline(), "linux-x86_64", "core"
            )["status"],
            "pass",
        )
        with self.assertRaises(MODULE.PerformanceRegression):
            MODULE.evaluate(
                [measurement(111)], baseline(), "linux-x86_64", "core"
            )

    def test_metric_sets_duplicates_and_schema_fail_closed(self) -> None:
        for documents in (
            [{"schema_version": 1, "metrics": {"unknown": 1}}],
            [measurement(1), measurement(2)],
            [{"schema_version": 1, "metrics": {"latency_us": True}}],
        ):
            with self.subTest(documents=documents), self.assertRaises(ValueError):
                MODULE.evaluate(documents, baseline(), "linux-x86_64", "core")

    def test_soak_document_is_consumed_without_changing_its_contract(self) -> None:
        soak = {
            "status": "pass",
            "duration_seconds": 28800.0,
            "samples": 5760,
            "max_rss_bytes": 110,
            "rss_limit_bytes": 500,
        }
        self.assertEqual(
            MODULE.evaluate([soak], baseline(), "linux-x86_64", "soak")["status"],
            "pass",
        )

    def test_waiver_requires_label_and_substantive_named_section(self) -> None:
        substantive = (
            "The renderer intentionally trades a measured startup cost for deterministic "
            "cell shaping. Profiling and before-after traces are attached, and the fixed "
            "absolute product budget still passes on both release platforms."
        )
        result = MODULE.evaluate(
            [measurement(111)],
            baseline(),
            "linux-x86_64",
            "core",
            event=event(label=True, justification=substantive),
        )
        self.assertEqual(result["status"], "waived")
        for candidate in (
            event(label=False, justification=substantive),
            event(label=True, justification="TBD"),
            {"pull_request": {"labels": [{"name": "perf-waiver"}], "body": substantive}},
            {},
        ):
            with self.subTest(candidate=candidate), self.assertRaises(
                MODULE.PerformanceRegression
            ):
                MODULE.evaluate(
                    [measurement(111)],
                    baseline(),
                    "linux-x86_64",
                    "core",
                    event=candidate,
                )

    def test_malformed_baseline_cannot_be_waived(self) -> None:
        malformed = baseline()
        malformed["maximum_regression_fraction"] = 0.11
        with self.assertRaisesRegex(ValueError, "0.10"):
            MODULE.evaluate(
                [measurement(1)],
                malformed,
                "linux-x86_64",
                "core",
                event=event(label=True, justification="valid " * 30),
            )


if __name__ == "__main__":
    unittest.main()
