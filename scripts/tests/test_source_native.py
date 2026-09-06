"""Source UI acceptance consumes prepared native provenance before execution."""
import os
from pathlib import Path
import subprocess
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]


class SourceNativeContractTests(unittest.TestCase):
    def test_source_verifier_refuses_missing_relative_and_unreceipted_paths(self):
        for value in ("", "libopentui.so", "/tmp/no-rw-native-receipt/libopentui.so"):
            with self.subTest(value=value):
                result = subprocess.run(
                    ["python3", str(ROOT / "scripts/verify-opentui-native.py")],
                    env={**os.environ, "ROTTWEILER_OPENTUI_LIBRARY": value},
                    capture_output=True, text=True, check=False,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")
                self.assertIn("Prepare and export ROTTWEILER_OPENTUI_LIBRARY", result.stderr)

    def test_workflow_source_suites_have_explicit_native_preparation(self):
        for name in ("ci", "nightly", "performance", "quality", "release"):
            workflow = yaml.safe_load((ROOT / f".github/workflows/{name}.yml").read_text())
            for job_name, job in workflow["jobs"].items():
                commands = [step.get("run", "") for step in job.get("steps", [])]
                source_suite = any("test:perf" in command or "ci_inventory.py package" in command
                                   or "cargo test --locked --workspace" in command
                                   or "cargo llvm-cov" in command for command in commands)
                if not source_suite:
                    continue
                with self.subTest(workflow=name, job=job_name):
                    preparation = next((i for i, command in enumerate(commands)
                                        if "scripts/build-opentui-native.py" in command
                                        and 'ROTTWEILER_OPENTUI_LIBRARY=$library' in command), None)
                    self.assertIsNotNone(preparation)
                    suite = next(i for i, command in enumerate(commands)
                                 if "test:perf" in command or "ci_inventory.py package" in command
                                 or "cargo test --locked --workspace" in command or "cargo llvm-cov" in command)
                    self.assertLess(preparation, suite)
