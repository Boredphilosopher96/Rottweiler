from __future__ import annotations

import os
import json
import shutil
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import m4_gate_support as support
from perf_process_scope import UnsettledScope
from perf_process_wait import observe_exit


class M4ProcessOwnershipTests(unittest.TestCase):
    def spawn(self, source):
        return support.spawn_pty(Path(sys.executable), dict(os.environ), Path.cwd(), ['-c', source])

    def test_observation_keeps_identity_until_idempotent_cleanup(self):
        process = self.spawn("print('READY',flush=True); raise SystemExit(7)")
        try:
            support.read_until(process, b'READY')
            status = support.wait_for_pty_exit(process, 3)
            self.assertEqual(os.waitstatus_to_exitcode(status), 7)
            self.assertEqual(observe_exit(process.pid), 7)
            support.stop_pty(process)
            self.assertTrue(process.reaped)
            with patch.object(support, 'signal_owned_group', side_effect=AssertionError('stale signal')):
                support.stop_pty(process)
        finally:
            support.stop_pty(process)

    def test_marker_failure_does_not_reap_before_cleanup(self):
        process = self.spawn('raise SystemExit(9)')
        try:
            with self.assertRaisesRegex(RuntimeError, 'exited with wait status'):
                support.read_until(process, b'NEVER', timeout=1)
            self.assertEqual(observe_exit(process.pid), 9)
        finally:
            support.stop_pty(process)

    def test_runtime_observed_exit_is_still_owned_for_group_settlement(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            child = subprocess.Popen([sys.executable, '-c', 'pass'], start_new_session=True)
            log = support.EngineErrorLog(path / 'error')
            log.close_input()
            runtime = support.Runtime(child, path / 'socket', path / 'token', log)
            try:
                import time
                while observe_exit(child.pid) is None:
                    time.sleep(.001)
                support.stop_runtime(runtime)
                self.assertEqual(child.returncode, 0)
            finally:
                if child.returncode is None:
                    os.kill(child.pid, signal.SIGKILL)
                    child.wait()

    def test_successful_gate_acknowledges_only_after_actual_pty_group_closes(self):
        from perf_process import run_sample
        module = Path(__file__).resolve().parents[2] / "crates/rw-cli/tests/m4_release_gate.py"
        source = f"""import importlib.util,sys,os
from pathlib import Path
from types import SimpleNamespace
spec=importlib.util.spec_from_file_location('m4', {str(module)!r})
m=importlib.util.module_from_spec(spec);sys.modules['m4']=m;spec.loader.exec_module(m)
m.parse_args=lambda: SimpleNamespace(evidence_json=None,metrics_json=None)
def gate(args,evidence):
    p=m.spawn_pty(Path(sys.executable),dict(os.environ),Path.cwd(),['-c',"print('READY',flush=True)"])
    try:
        m.read_until(p,b'READY')
    finally:
        m.stop_pty(p)
    assert p.reaped and p.group_settled
    return 0
m.run_gate=gate
assert m.main() == 0
"""
        result = run_sample([sys.executable, '-c', source], cwd=Path.cwd(), env=dict(os.environ), delegated=True)
        self.assertEqual(result.returncode, 0)

    def test_gate_cancellation_retains_scratch_and_has_no_success_acknowledgement(self):
        from perf_process import run_sample
        module = Path(__file__).resolve().parents[2] / "crates/rw-cli/tests/m4_release_gate.py"
        for cancel in (False, True):
            with self.subTest(cancel=cancel), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                evidence = root / "evidence.json"
                pid_file = root / "child.pid"
                source = f"""import importlib.util,sys,os
from pathlib import Path
from types import SimpleNamespace
spec=importlib.util.spec_from_file_location('m4', {str(module)!r})
m=importlib.util.module_from_spec(spec);sys.modules['m4']=m;spec.loader.exec_module(m)
m.parse_args=lambda: SimpleNamespace(evidence_json=Path({str(evidence)!r}),metrics_json=None)
def gate(args,evidence):
    with m.gate_scratch(evidence):
        p=m.spawn_pty(Path(sys.executable),dict(os.environ),Path.cwd(),['-c',"import time; print('READY',flush=True); time.sleep(60)"])
        Path({str(pid_file)!r}).write_text(str(p.pid))
        try:
            m.read_until(p,b'READY')
            if {cancel!r}: m.read_until(p,b'NEVER',timeout=30)
            raise RuntimeError('gate assertion')
        finally:
            m.stop_pty(p)
m.run_gate=gate
m.main()
"""
                retained = None
                try:
                    with self.assertRaises(UnsettledScope):
                        run_sample([sys.executable, '-c', source], cwd=Path.cwd(), env=dict(os.environ),
                                   timeout=.5 if cancel else 5, delegated=True)
                    result = json.loads(evidence.read_text())
                    retained = Path(result['retained_scratch'])
                    self.assertTrue(retained.is_dir())
                    self.assertEqual(result['status'], 'fail')
                    with self.assertRaises(ProcessLookupError):
                        os.killpg(int(pid_file.read_text()), 0)
                finally:
                    if retained is not None:
                        shutil.rmtree(retained)

    def test_failed_descendant_diagnostic_still_settles_the_owned_group(self):
        process = self.spawn("import time; print('READY',flush=True); time.sleep(60)")
        support.read_until(process, b'READY')
        try:
            with patch.object(support, 'descendant_pids', side_effect=RuntimeError('diagnostic failed')):
                with self.assertRaisesRegex(RuntimeError, 'diagnostic failed'):
                    support.terminate_process_tree(process)
            self.assertTrue(process.group_settled)
        finally:
            support.stop_pty(process)

    def test_descendant_snapshot_is_never_signal_authority(self):
        process = self.spawn("import time; print('READY',flush=True); time.sleep(60)")
        support.read_until(process, b'READY')
        signals = []
        original = support.signal_owned_group
        def signal_group(pid, number):
            signals.append(pid)
            original(pid, number)
        try:
            # A reused/unrelated PID can cause a conservative failure, but must
            # never receive a signal from this diagnostic descendant snapshot.
            with patch.object(support, 'descendant_pids', return_value=[1]), \
                    patch.object(support, 'process_exists', return_value=True), \
                    patch.object(support, 'signal_owned_group', side_effect=signal_group):
                with self.assertRaisesRegex(UnsettledScope, 'descendants'):
                    support.terminate_process_tree(process, timeout=.1)
            self.assertEqual(signals, [process.pid, process.pid])
            self.assertTrue(process.reaped)
        finally:
            support.stop_pty(process)


if __name__ == '__main__':
    unittest.main()
