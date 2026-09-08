"""A failed physical proof must preserve evidence without a successful acknowledgement."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

BRIDGE = Path(__file__).resolve().parents[2] / "packages/tui/test/support/owned-process.py"
spec = importlib.util.spec_from_file_location("tui_process_bridge", BRIDGE)
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


class TuiProcessBridgeTests(unittest.TestCase):
    def test_failed_group_closure_retains_original_phase_and_denies_acknowledgement(self):
        with tempfile.TemporaryDirectory() as directory:
            request = Path(directory) / "request.json"
            result = Path(directory) / "result.json"
            request.write_text(json.dumps({"command": ["unused"], "cwd": directory,
                                           "env": {}, "timeoutMs": 100, "maxOutputBytes": 1024}))

            def unsettled(*_args, **kwargs):
                kwargs["log"].write(b"bounded native evidence")
                raise RuntimeError("UNSETTLED process group: leader=123 phase=after-reap")

            with patch.object(bridge, "run_sample", side_effect=unsettled), patch.object(
                bridge, "require_sample_settlement", side_effect=RuntimeError("active owner")
            ), self.assertRaises(SystemExit) as failure:
                bridge.run(request, result)
            self.assertEqual(failure.exception.code, 125)
            evidence = json.loads(result.read_text())
            self.assertIs(evidence["settled"], False)
            self.assertIn("leader=123 phase=after-reap", evidence["error"])
            self.assertIn("active owner", evidence["error"])
            self.assertIn("bounded native evidence", evidence["error"])
            self.assertLess(result.stat().st_size, 1024)


if __name__ == "__main__":
    unittest.main()
