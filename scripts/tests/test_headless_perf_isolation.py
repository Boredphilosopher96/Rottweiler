from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[2]


class HeadlessPerformanceIsolationTests(unittest.TestCase):
    def test_ci_builds_and_checksums_macos_binary_in_noindex_runner_temp(self) -> None:
        workflow = (REPO / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        build = workflow.split("  macos-performance-build:", 1)[1].split(
            "  performance:", 1
        )[0]

        self.assertIn("needs: [test, security-tests, performance-smoke]", build)
        self.assertIn("$RUNNER_TEMP/rottweiler-macos-performance-build.noindex", build)
        self.assertIn("$RUNNER_TEMP/rottweiler-macos-performance-artifact.noindex", build)
        self.assertIn("shasum -a 256 rw > rw.sha256", build)
        self.assertIn("actions/upload-artifact@043fb46d", build)
        self.assertIn("if-no-files-found: error", build)
        self.assertIn("overwrite: true", build)

    def test_ci_performance_verifies_and_uses_prebuilt_only_on_macos(self) -> None:
        workflow = (REPO / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        performance = workflow.split("  performance:", 1)[1].split(
            "  m4-ssh-loopback:", 1
        )[0]

        self.assertIn(
            "needs: [test, security-tests, macos-performance-build]", performance
        )
        self.assertIn("actions/download-artifact@37930b1c", performance)
        self.assertIn("shasum -a 256 -c rw.sha256", performance)
        self.assertEqual(performance.count("ROTTWEILER_PERF_PREBUILT_RW:"), 1)
        self.assertIn("Headless performance gate (Linux source build)", performance)
        self.assertIn(
            "pr-performance-${{ matrix.platform }}-${{ github.run_id }}-${{ github.run_attempt }}",
            performance,
        )
        self.assertLess(
            performance.index("Headless performance gate (macOS prebuilt binary)"),
            performance.index("Install Rust toolchain"),
        )

    def test_gate_preserves_fixed_sampling_and_writes_evidence_first(self) -> None:
        gate = (REPO / "crates/rw-cli/tests/perf_gate.sh").read_text(
            encoding="utf-8"
        )

        self.assertIn('smoke = os.environ.get("ROTTWEILER_PERF_SMOKE") == "1"', gate)
        self.assertIn('"100" if smoke else "500"', gate)
        self.assertIn("minimum_samples = 100", gate)
        self.assertIn('60 if sys.platform == "darwin" else 1', gate)
        self.assertIn("for index in range(-5, 0)", gate)
        self.assertNotIn("warmup_count", gate)
        self.assertIn("if smoke and turn_p95 >= 20", gate)
        self.assertIn("if smoke and turn_p99 >= 40", gate)
        self.assertIn("if not smoke and turn_p99 >= 20", gate)
        self.assertIn("sample_count > 5000", gate)
        self.assertIn('"samples": [', gate)
        self.assertIn('"runner": {', gate)
        self.assertIn('f"{output.stem}.evidence{output.suffix}"', gate)
        self.assertIn("source_metadata = os.fstat(source.fileno())", gate)
        self.assertIn("source_metadata.st_nlink != 1", gate)
        self.assertLess(
            gate.index("evidence_temporary.replace(evidence)"),
            gate.index("if start_p99 >= 80"),
        )

    def test_prebuilt_gate_keeps_metrics_schema_and_writes_ordered_evidence(self) -> None:
        gate = REPO / "crates/rw-cli/tests/perf_gate.sh"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "rw"
            binary.write_text(
                "#!/bin/sh\n"
                "printf 'ready\\n'\n"
                "printf 'rw_perf_zero_latency_turn_us=1000\\n' >&2\n",
                encoding="utf-8",
            )
            binary.chmod(0o700)
            site = root / "site"
            site.mkdir()
            (site / "sitecustomize.py").write_text(
                "import time\ntime.sleep = lambda _seconds: None\n", encoding="utf-8"
            )
            output = root / "results" / "headless.json"
            env = {
                **os.environ,
                "GITHUB_ACTIONS": "true",
                "PYTHONPATH": str(site),
                "ROTTWEILER_PERF_OUTPUT": str(output),
                "ROTTWEILER_PERF_PREBUILT_RW": str(binary),
                "ROTTWEILER_PERF_SAMPLES": "100",
                "RUNNER_TEMP": str(root),
            }

            subprocess.run([str(gate)], cwd=REPO, env=env, check=True)

            metrics = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(set(metrics), {"schema_version", "metrics"})
            evidence_path = output.with_name("headless.evidence.json")
            evidence = json.loads(evidence_path.read_text(encoding="utf-8"))
            self.assertEqual(
                set(evidence), {"schema_version", "sample_count", "samples", "runner"}
            )
            self.assertEqual(evidence["sample_count"], 100)
            self.assertEqual(
                [sample["index"] for sample in evidence["samples"]], list(range(100))
            )
            self.assertTrue(
                all(
                    set(sample)
                    == {"index", "headless_print_us", "turn_overhead_us"}
                    for sample in evidence["samples"]
                )
            )


if __name__ == "__main__":
    unittest.main()
