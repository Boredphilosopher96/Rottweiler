"""Real child ownership regressions; these do not measure product performance."""
from __future__ import annotations

import os
import json
import shutil
from pathlib import Path
import pty
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
from perf_process_scope import UnsettledScope
from perf_process_wait import observe_stopped
from soak_process import terminate_supervisor, wait_with_terminal, kill_direct_child
from perf_scratch import retained_scratch


class SoakProcessTests(unittest.TestCase):
    def spawn(self, body: str):
        master, slave = pty.openpty()
        os.set_blocking(master, False)
        process = subprocess.Popen([sys.executable, "-c", body], stdin=slave,
                                   stdout=slave, stderr=slave, start_new_session=True)
        os.close(slave)
        self.addCleanup(os.close, master)
        def cleanup():
            if process.returncode is None:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait(timeout=2)
        self.addCleanup(cleanup)
        return process, master

    def test_zero_exit_after_detached_child_reap_proves_closure(self):
        with tempfile.TemporaryDirectory() as temporary:
            ready = Path(temporary) / "ready"
            process, master = self.spawn(
                "import signal,subprocess,sys,time\n"
                "child = subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)'], start_new_session=True)\n"
                "def stop(*_):\n"
                "    child.terminate()\n"
                "    child.wait(timeout=2)\n"
                "    raise SystemExit(0)\n"
                "signal.signal(signal.SIGTERM, stop)\n"
                f"open({str(ready)!r},'w').write(str(child.pid))\n"
                "while True: time.sleep(.01)\n")
            self.wait_ready(ready)
            child_pid = int(ready.read_text())
            terminate_supervisor(process, master, {child_pid}, lambda: {process.pid, child_pid}, grace=.5)
            with self.assertRaises(ProcessLookupError):
                os.kill(child_pid, 0)
            self.assertEqual(process.returncode, 0)

    def wait_ready(self, path):
        import time
        deadline = time.monotonic() + 2
        while not path.exists():
            if time.monotonic() > deadline:
                self.fail("fixture readiness timeout")
            time.sleep(.005)

    def test_failed_supervisor_never_signals_observed_detached_process(self):
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"],
                                 start_new_session=True)
        self.addCleanup(lambda: child.wait(timeout=2))
        self.addCleanup(child.kill)
        process, master = self.spawn("raise SystemExit(7)")
        self.assertEqual(wait_with_terminal(process, master, 2), 7)
        with tempfile.TemporaryDirectory() as temporary:
            evidence = []
            with self.assertRaises(UnsettledScope):
                with retained_scratch("soak-", parent=Path(temporary), evidence=evidence.append) as scratch:
                    (scratch / "journal").write_text("retained")
                    terminate_supervisor(process, master, {child.pid}, lambda: {child.pid}, grace=.05)
            self.assertEqual((evidence[0] / "journal").read_text(), "retained")
        self.assertIsNone(child.poll())
        self.assertEqual(process.returncode, 7)

    def test_missing_observation_refuses_success_after_clean_supervisor_exit(self):
        with tempfile.TemporaryDirectory() as temporary:
            ready = Path(temporary) / "ready"
            process, master = self.spawn(
                "import signal,time\n"
                "signal.signal(signal.SIGTERM, lambda *_: exit(0))\n"
                f"open({str(ready)!r},'w').write('ready')\n"
                "while True: time.sleep(.01)\n")
            self.wait_ready(ready)
            with self.assertRaisesRegex(UnsettledScope, "process observation"):
                terminate_supervisor(process, master, set(),
                                     mock.Mock(side_effect=RuntimeError("ps failed")), grace=.5)
            self.assertEqual(process.returncode, 0)

    def test_reaped_leader_is_never_signalled(self):
        process, master = self.spawn("pass")
        process.wait(timeout=2)
        with mock.patch("soak_process.os.kill") as kill, self.assertRaises(UnsettledScope):
            terminate_supervisor(process, master, set(), lambda: set())
        kill.assert_not_called()

    def test_forced_shutdown_is_unsettled_even_without_descendants(self):
        with tempfile.TemporaryDirectory() as temporary:
            ready = Path(temporary) / "ready"
            process, master = self.spawn(
                "import signal,time\n"
                "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
                f"open({str(ready)!r},'w').write('ready')\n"
                "while True: time.sleep(.01)\n")
            self.wait_ready(ready)
            with self.assertRaisesRegex(UnsettledScope, "successful supervisor cleanup"):
                terminate_supervisor(process, master, set(), lambda: {process.pid}, grace=.02)
            self.assertEqual(process.returncode, -signal.SIGKILL)

    def test_fault_pins_direct_child_then_resumes_parent_to_reap(self):
        with tempfile.TemporaryDirectory() as temporary:
            ready = Path(temporary) / "ready"
            reaped = Path(temporary) / "reaped"
            process, master = self.spawn(
                "import signal,subprocess,sys,time\n"
                "child = subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)'])\n"
                "signal.signal(signal.SIGTERM, lambda *_: exit(0))\n"
                f"open({str(ready)!r},'w').write(str(child.pid))\n"
                "child.wait()\n"
                f"open({str(reaped)!r},'w').write(str(child.returncode))\n"
                "while True: time.sleep(.01)\n")
            self.wait_ready(ready)
            child_pid = int(ready.read_text())
            def select_child():
                self.assertTrue(observe_stopped(process.pid))
                self.assertTrue(observe_stopped(process.pid))
                return child_pid
            self.assertEqual(kill_direct_child(process, select_child), child_pid)
            self.wait_ready(reaped)
            self.assertEqual(reaped.read_text(), str(-signal.SIGKILL))
            terminate_supervisor(process, master, set(), lambda: {process.pid}, grace=.5)

    def test_fault_selector_failure_resumes_owned_supervisor(self):
        with tempfile.TemporaryDirectory() as temporary:
            ready = Path(temporary) / "ready"
            process, master = self.spawn(
                "import signal,time\n"
                "signal.signal(signal.SIGTERM, lambda *_: exit(0))\n"
                f"open({str(ready)!r},'w').write('ready')\n"
                "while True: time.sleep(.01)\n")
            self.wait_ready(ready)
            with self.assertRaisesRegex(RuntimeError, "unavailable"):
                kill_direct_child(process, mock.Mock(side_effect=RuntimeError("unavailable")))
            terminate_supervisor(process, master, set(), lambda: {process.pid}, grace=.5)

    def test_direct_gate_sigterm_reaps_supervisor_and_retains_failure_scratch(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            ready, result = root / "ready", root / "result.json"
            rw, host = root / "rw", root / "rottweiler-js-host"
            rw.write_text(
                f"#!{sys.executable}\nimport os,signal,time\n"
                "signal.signal(signal.SIGTERM, lambda *_: exit(0))\n"
                f"open({str(ready)!r},'w').write(str(os.getpid()))\n"
                "while True: time.sleep(.01)\n")
            host.write_text(f"#!{sys.executable}\n")
            rw.chmod(0o700)
            host.chmod(0o700)
            gate = subprocess.Popen([sys.executable, str(SCRIPTS / "run-soak.py"),
                                     "--rw", str(rw), "--duration-seconds", "20",
                                     "--output", str(result)], stdout=subprocess.DEVNULL,
                                    stderr=subprocess.DEVNULL, start_new_session=True)
            try:
                self.wait_ready(ready)
                supervisor = int(ready.read_text())
                gate.send_signal(signal.SIGTERM)
                self.assertEqual(gate.wait(timeout=5), 1)
                with self.assertRaises(ProcessLookupError):
                    os.kill(supervisor, 0)
                evidence = json.loads(result.read_text())
                self.assertIn(evidence["status"], ("fail", "UNSETTLED"))
                retained = Path(evidence["retained_scratch"])
                self.assertTrue(retained.is_dir())
                shutil.rmtree(retained)
            finally:
                if gate.returncode is None:
                    gate.kill()
                    gate.wait(timeout=2)


if __name__ == "__main__":
    unittest.main()
