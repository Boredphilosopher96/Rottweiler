from __future__ import annotations

import copy
import importlib.util
import json
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch

REPO = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("client_input_probe", REPO / "packages/tui/scripts/client-input-probe.py")
assert SPEC is not None and SPEC.loader is not None
PROBE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PROBE)


def report() -> dict:
    return {"schemaVersion": 1, "keysPerTrial": 128, "warmupKeysExcludedPerTrial": 5,
            "budgetMs": 16, "width": 110, "height": 36, "maximumComposerUtf8Bytes": 131072,
            "trials": [{"samplesMs": [2.0] * 128, "p99Ms": 2.0, "exactContent": True,
                        "nativeFrameContainsInput": True, "finalUtf8Bytes": 130944,
                        "allocationBytes": 1024} for _ in range(3)],
            "terminal": {"queuedBytes": 0, "bytes": 100}, "finalAllocationBytes": 0,
            "failure": None, "passed": True}


class ClientInputProbeTests(unittest.TestCase):
    def test_raw_trial_failure_cannot_be_hidden_by_passing_flag(self) -> None:
        good = report()
        PROBE.validate(good)
        for field, value in (("samplesMs", [17.0] * 128), ("samplesMs", [float("nan")] * 128),
                             ("samplesMs", [2.0] * 127), ("p99Ms", 1), ("exactContent", False),
                             ("nativeFrameContainsInput", False), ("finalUtf8Bytes", 128),
                             ("allocationBytes", 0)):
            with self.subTest(field=field, value=str(value)[:30]):
                bad = copy.deepcopy(good)
                bad["trials"][1][field] = value
                with self.assertRaises(ValueError):
                    PROBE.validate(bad)
        for field, value in (("budgetMs", 20), ("finalAllocationBytes", 1), ("failure", "lost state"),
                             ("terminal", {"queuedBytes": 1, "bytes": 100}), ("trials", [])):
            with self.subTest(field=field):
                bad = copy.deepcopy(good)
                bad[field] = value
                with self.assertRaises(ValueError):
                    PROBE.validate(bad)

    def test_runner_uses_verified_shared_host_with_explicit_role_and_private_environment(self) -> None:
        receipt = {"identity_sha256": "identity", "identity": {"source": {"commit": "exact"}},
                   "components": {"js_host": {"path": "bin/rottweiler-js-host"}}}
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            candidate, output = root / "candidate", root / "evidence"

            def launch(argv: list, **options):
                self.assertEqual(argv, [str(candidate / "bin/rottweiler-js-host"), PROBE.TUI_ROLE])
                self.assertEqual(options["timeout"], 120)
                self.assertNotIn("ROTTWEILER_PERF_SMOKE", options["env"])
                self.assertNotIn("ROTTWEILER_CLIENT_MEMORY_PROBE_REPORT", options["env"])
                self.assertEqual(Path(options["env"]["ROTTWEILER_CLIENT_INPUT_PROBE_DIRECTORY"]), options["cwd"])
                Path(options["env"]["ROTTWEILER_CLIENT_INPUT_PROBE_REPORT"]).write_text(json.dumps(report()))
                return SimpleNamespace(returncode=0)

            with patch.object(PROBE.native_candidate, "verify", return_value=receipt) as verify, \
                    patch.object(PROBE.subprocess, "run", side_effect=launch), \
                    patch.dict(PROBE.os.environ, {"ROTTWEILER_PERF_SMOKE": "1", "ROTTWEILER_CLIENT_MEMORY_PROBE_REPORT": "wrong"}):
                PROBE.run(candidate, output)
            verify.assert_called_once_with(candidate, REPO)
            summary = json.loads((output / "summary.json").read_text())
            self.assertEqual(summary["candidate_identity"], "identity")
            self.assertEqual(len(summary["process"]["trials"]), 3)

            # A failed run cannot reuse that successful raw report.
            with patch.object(PROBE.native_candidate, "verify", return_value=receipt), \
                    patch.object(PROBE.subprocess, "run", return_value=SimpleNamespace(returncode=1)):
                with self.assertRaises(ValueError):
                    PROBE.run(candidate, output)
            self.assertIsNone(json.loads((output / "summary.json").read_text())["process"])


if __name__ == "__main__":
    unittest.main()
