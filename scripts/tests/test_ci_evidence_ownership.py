"""Actual CI-wrapper cancellation composes with nested process ownership."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
from perf_process import run_sample
from perf_process_scope import UnsettledScope


class EvidenceOwnershipTests(unittest.TestCase):
    def command(self, output, child, delegated=False):
        return [sys.executable, str(SCRIPTS / "ci_evidence.py"),
                *(["--delegated"] if delegated else []), "--gate", "ownership",
                "--output", str(output), "--", sys.executable, "-c", child]

    def test_timeout_settles_independent_nested_session_before_ci_ack(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            output, pid = directory / "result.json", directory / "child.pid"
            native = f"import os,time; open({str(pid)!r},'w').write(str(os.getpid())); time.sleep(60)"
            child = ("import os,sys\nfrom pathlib import Path\nfrom perf_process import run_sample\n"
                     f"run_sample([sys.executable,'-c',{native!r}],cwd=Path.cwd(),env=dict(os.environ),timeout=60)\n")
            with self.assertRaises(TimeoutError) as failure:
                run_sample(self.command(output, child, True), cwd=directory,
                           env=dict(os.environ, PYTHONPATH=str(SCRIPTS)), timeout=.6,
                           delegated=True)
            self.assertNotIsInstance(failure.exception, UnsettledScope)
            with self.assertRaises(ProcessLookupError):
                os.kill(int(pid.read_text()), 0)
            evidence = json.loads(output.read_text())
            self.assertEqual(evidence["exit_code"], 130)
            self.assertNotIn("cleanup_error", evidence)

    def test_missing_nested_ack_cannot_publish_success(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "result.json"
            run = subprocess.run(self.command(output, "print('finished')", True),
                                 capture_output=True, timeout=20, check=False)
            self.assertEqual(run.returncode, 1)
            evidence = json.loads(output.read_text())
            self.assertEqual(evidence["status"], "failed")
            self.assertIn("UNSETTLED", evidence["cleanup_error"])

    def test_nested_forced_exit_preserves_unsettled_child_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "result.json"
            child = ("import os\nfrom perf_process import _SCOPE\n"
                     "token=_SCOPE.starting(); _SCOPE.started(token,1); os._exit(0)\n")
            run = subprocess.run(self.command(output, child, True), capture_output=True,
                                 env=dict(os.environ, PYTHONPATH=str(SCRIPTS)), timeout=20, check=False)
            self.assertEqual(run.returncode, 1)
            evidence = json.loads(output.read_text())
            self.assertIn('active_children', evidence["cleanup_error"])
            self.assertIn(': 1', evidence["cleanup_error"])

    def test_signal_during_spawn_handoff_does_not_abandon_child(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            output, pid = directory / "result.json", directory / "child.pid"
            child = "import time; time.sleep(60)"
            wrapper = (
                "import os,signal,subprocess,sys\nfrom pathlib import Path\n"
                "import ci_evidence,perf_process_owner\n"
                "original=perf_process_owner.subprocess.Popen\n"
                "def spawn(command,**kwargs):\n"
                " process=original(command,**kwargs)\n"
                " if kwargs.get('start_new_session'):\n"
                f"  Path({str(pid)!r}).write_text(str(process.pid))\n"
                "  os.kill(os.getpid(),signal.SIGTERM)\n"
                " return process\n"
                "perf_process_owner.subprocess.Popen=spawn\n"
                f"raise SystemExit(ci_evidence.observe([sys.executable,'-c',{child!r}],'handoff',Path({str(output)!r})))\n"
            )
            run = subprocess.run([sys.executable, "-c", wrapper], capture_output=True,
                                 env=dict(os.environ, PYTHONPATH=str(SCRIPTS)), timeout=20, check=False)
            self.assertEqual(run.returncode, 130, run.stderr.decode())
            with self.assertRaises(ProcessLookupError):
                os.kill(int(pid.read_text()), 0)
            self.assertNotIn("cleanup_error", json.loads(output.read_text()))
