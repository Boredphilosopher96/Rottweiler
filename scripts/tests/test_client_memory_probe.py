from __future__ import annotations

import importlib.util
import json
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch

REPO = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("client_memory_probe", REPO / "packages/tui/scripts/client-memory-probe.py")
assert SPEC is not None and SPEC.loader is not None
PROBE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PROBE)


class ClientMemoryProbeTests(unittest.TestCase):
    def test_probe_refuses_stale_evidence_without_overwriting_it(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "evidence"
            output.mkdir()
            retained = output / "summary.json"
            retained.write_text("retained failure evidence")
            receipt = {"components": {"js_host": {"path": "host"}}}
            with patch.object(PROBE.native_candidate, "verify", return_value=receipt), \
                    patch.object(PROBE.subprocess, "run") as launch:
                for action in (lambda: PROBE.run(root, output, 2, 3),
                               lambda: PROBE.run_held(root, output, 2, "output")):
                    with self.assertRaises(FileExistsError):
                        action()
                launch.assert_not_called()
            self.assertEqual(retained.read_text(), "retained failure evidence")

    def test_held_probe_owns_its_mode_and_preserves_raw_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "evidence"
            receipt = {"components": {"js_host": {"path": "host"}}}

            def launch(argv, **options):
                environment = options["env"]
                self.assertEqual(argv, [str(root / "host"), PROBE.TUI_ROLE])
                self.assertNotIn("ROTTWEILER_PERF_SMOKE", environment)
                self.assertNotIn("ROTTWEILER_CLIENT_INPUT_PROBE_REPORT", environment)
                self.assertNotIn("ROTTWEILER_CLIENT_MEMORY_PROBE_RECYCLE", environment)
                self.assertEqual(environment["ROTTWEILER_CLIENT_MEMORY_HELD_VIEW"], "review")
                Path(environment["ROTTWEILER_CLIENT_MEMORY_PROBE_REPORT"]).write_text(json.dumps({"failure": "retained"}))
                return SimpleNamespace(returncode=1)

            inherited = {"ROTTWEILER_PERF_SMOKE": "1", "ROTTWEILER_CLIENT_INPUT_PROBE_REPORT": "wrong",
                         "ROTTWEILER_CLIENT_MEMORY_PROBE_RECYCLE": "1"}
            with patch.object(PROBE.native_candidate, "verify", return_value=receipt), \
                    patch.object(PROBE.subprocess, "run", side_effect=launch), \
                    patch.dict(PROBE.os.environ, inherited):
                with self.assertRaisesRegex(ValueError, "exited 1"):
                    PROBE.run_held(root, output, 2, "review")
            self.assertEqual(json.loads((output / "held-review.json").read_text()), {"failure": "retained"})


if __name__ == "__main__":
    unittest.main()
