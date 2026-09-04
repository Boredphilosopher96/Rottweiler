import json
import importlib.util
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / "ci_evidence.py"
SPEC = importlib.util.spec_from_file_location("ci_evidence", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class CiEvidenceTests(unittest.TestCase):
    def test_permission_denial_requires_proof_that_no_live_members_remain(self):
        process = subprocess.Popen([sys.executable, "-c", "pass"])
        process.wait()
        with patch.object(MODULE.os, "killpg", side_effect=PermissionError), patch.object(
            MODULE.subprocess, "check_output", return_value=f"{process.pid} S\n".encode(),
        ):
            with self.assertRaises(PermissionError):
                MODULE.settle_group(process)
        with patch.object(MODULE.os, "killpg", side_effect=PermissionError), patch.object(
            MODULE.subprocess, "check_output", return_value=f"{process.pid} Z\n".encode(),
        ):
            MODULE.settle_group(process)
        with patch.object(MODULE.subprocess, "check_output", return_value=b"unavailable"):
            with self.assertRaises(OSError):
                MODULE.group_has_live_members(process.pid)

    @unittest.skipUnless(hasattr(os, "fork"), "requires Unix process groups")
    def test_cancellation_reaps_group_after_its_leader_has_exited(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            pidfile = root / "descendant.pid"
            heartbeat = root / "heartbeat"
            output = root / "result.json"
            child = (
                "import os,pathlib,time\n"
                "leader = os.getpid()\n"
                "if os.fork(): os._exit(0)\n"
                "while os.getppid() == leader: time.sleep(.01)\n"
                f"pathlib.Path({str(pidfile)!r}).write_text(str(os.getpid()))\n"
                "while True:\n"
                f" pathlib.Path({str(heartbeat)!r}).write_text(str(time.monotonic_ns()))\n"
                " time.sleep(.03)\n"
            )
            wrapper = subprocess.Popen([
                sys.executable, str(SCRIPT), "--gate", "fixture", "--output", str(output), "--",
                sys.executable, "-c", child,
            ], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            descendant = None
            try:
                deadline = time.monotonic() + 5
                while not heartbeat.exists() and time.monotonic() < deadline:
                    time.sleep(.01)
                self.assertTrue(pidfile.exists(), "descendant did not start")
                descendant = int(pidfile.read_text())
                wrapper.terminate()
                _, errors = wrapper.communicate(timeout=10)
                self.assertEqual(wrapper.returncode, 130, errors.decode())
                before = heartbeat.read_text()
                time.sleep(.15)
                self.assertEqual(heartbeat.read_text(), before, "cancelled gate left a running descendant")
                self.assertEqual(json.loads(output.read_text())["status"], "failed")
            finally:
                if wrapper.poll() is None:
                    wrapper.kill()
                    wrapper.wait()
                if descendant is not None:
                    try:
                        os.kill(descendant, signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    def test_secret_split_across_output_reads_is_redacted(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "result.json"
            secret = "rw-fixture-secret-1234567890"
            child = "import os,time; os.write(1,b'rw-fixture-'); time.sleep(.1); os.write(1,b'secret-1234567890\\n')"
            result = subprocess.run([
                sys.executable, str(SCRIPT), "--gate", "fixture", "--output", str(output), "--",
                sys.executable, "-c", child,
            ], env=os.environ | {"RW_FIXTURE_SECRET": secret}, capture_output=True, check=False)
            self.assertEqual(result.returncode, 0)
            self.assertNotIn(secret, result.stdout.decode())
            self.assertNotIn(secret, output.read_text())
            self.assertIn("[REDACTED]", result.stdout.decode())

    def test_failure_preserves_exit_status_and_bounded_tail(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "result.json"
            result = subprocess.run([sys.executable, str(SCRIPT), "--gate", "fixture", "--output", str(output), "--",
                                     sys.executable, "-c", "import sys; print('x' * 200000); print('final marker'); sys.exit(7)"],
                                    stdout=subprocess.DEVNULL, check=False)
            self.assertEqual(result.returncode, 7)
            evidence = json.loads(output.read_text())
            self.assertEqual(evidence["exit_code"], 7)
            self.assertEqual(evidence["status"], "failed")
            self.assertLessEqual(len(evidence["log_tail"].encode()), 128 * 1024)
            self.assertIn("final marker", evidence["log_tail"])

    def test_launch_failure_still_writes_result(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "result.json"
            result = subprocess.run([sys.executable, str(SCRIPT), "--gate", "fixture", "--output", str(output), "--",
                                     "/nonexistent-rw-ci-command"], check=False)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("launch_error", json.loads(output.read_text()))
