"""A failed physical proof must preserve evidence without a successful acknowledgement."""
import importlib.util
import json
from pathlib import Path
import tempfile
import sys
import os
import subprocess
import time
import unittest
from unittest.mock import patch

BRIDGE = Path(__file__).resolve().parents[2] / "packages/tui/test/support/owned-process.py"
sys.path.insert(0, str(BRIDGE.parent))
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

    def test_parent_pipe_loss_cancels_and_reaps_native_work_before_result(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            request, result, pid = root / "request.json", root / "result.json", root / "pid"
            code = f"import os,time;open({str(pid)!r},'w').write(str(os.getpid()));time.sleep(30)"
            request.write_text(json.dumps({"command": [sys.executable, "-c", code], "cwd": directory,
                                           "env": {}, "timeoutMs": 5000, "maxOutputBytes": 1024}))
            supervisor = subprocess.Popen([sys.executable, str(BRIDGE), str(request), str(result)],
                                          stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
                                          stderr=subprocess.DEVNULL)
            try:
                deadline = time.monotonic() + 2
                while not pid.exists():
                    if time.monotonic() >= deadline:
                        self.fail("native fixture never became ready")
                    time.sleep(.005)
                supervisor.stdin.close()
                self.assertEqual(supervisor.wait(timeout=3), 0)
                evidence = json.loads(result.read_text())
                self.assertTrue(evidence["settled"])
                self.assertIn("ScopeCancelled", evidence["error"])
                self.assertEqual(evidence["supervisor_pid"], supervisor.pid)
                with self.assertRaises(ProcessLookupError):
                    os.kill(int(pid.read_text()), 0)
            finally:
                if supervisor.stdin is not None and not supervisor.stdin.closed:
                    supervisor.stdin.close()
                supervisor.wait(timeout=7)


if __name__ == "__main__":
    unittest.main()
