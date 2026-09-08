"""Physical PTY ownership and refusal are independent from product timings."""
import os
from pathlib import Path
import signal
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from m8_process import Terminal, append_bounded, OUTPUT_BYTES
from perf_process_wait import observe_exit, require_group_disappearance
from perf_process_scope import UnsettledScope, ScopeCancelled
from perf_process_owner import SCOPE
from perf_scratch import retained_scratch


class TerminalTests(unittest.TestCase):
    def spawn(self, body):
        terminal = Terminal([sys.executable, '-c', body], cwd=Path.cwd(), env=dict(os.environ))
        self.addCleanup(lambda: terminal.close() if not terminal.closed else None)
        return terminal

    def wait(self, terminal):
        deadline = time.monotonic() + 3
        output, errors = bytearray(), bytearray()
        while time.monotonic() < deadline:
            stdout, stderr = terminal.read()
            output.extend(stdout); errors.extend(stderr)
            if terminal.observe_exit() is not None:
                return bytes(output), bytes(errors)
        self.fail('fixture did not finish')

    def test_terminal_input_stderr_and_nonreaping_exit_observation(self):
        terminal = self.spawn("import sys; value=input(); print(value, flush=True); print('ERR',file=sys.stderr,flush=True)")
        terminal.write(b'exact\n', deadline=time.monotonic() + 2)
        output, errors = self.wait(terminal)
        self.assertIn(b'exact', output)
        self.assertIn(b'ERR', errors)
        self.assertEqual(observe_exit(terminal.pid), 0)
        terminal.close()
        with patch('perf_process_owner.signal_owned_group', side_effect=AssertionError('stale signal')):
            terminal.close()
        self.assertEqual(terminal.owner.process.returncode, 0)

    def test_cancellation_closes_physical_owner_before_scratch_cleanup(self):
        with tempfile.TemporaryDirectory() as root:
            evidence = []
            with self.assertRaises(ScopeCancelled):
                with retained_scratch('m8-', parent=Path(root), evidence=evidence.append) as scratch:
                    terminal = self.spawn('import time; time.sleep(30)')
                    try:
                        with patch.object(SCOPE, 'cancelled', signal.SIGTERM):
                            terminal.read()
                    finally:
                        terminal.close()
            self.assertTrue(evidence[0].exists())
            with self.assertRaises(ProcessLookupError):
                os.kill(terminal.pid, 0)

    def test_failed_group_proof_retains_scratch_and_never_resignals_reaped_pid(self):
        with tempfile.TemporaryDirectory() as root:
            evidence = []
            with self.assertRaises(UnsettledScope):
                with retained_scratch('m8-', parent=Path(root), evidence=evidence.append):
                    terminal = self.spawn('pass')
                    self.wait(terminal)
                    with patch('perf_process_owner.require_group_disappearance', side_effect=UnsettledScope('unknown descendant')):
                        terminal.close()
            self.assertTrue(evidence[0].exists())
            with patch('perf_process_owner.signal_owned_group', side_effect=AssertionError('stale signal')), self.assertRaises(UnsettledScope):
                terminal.close()
            # The injected proof failure is local to this test. Complete the
            # real absence proof before retiring its test-scope registration.
            require_group_disappearance(terminal.pid)
            SCOPE.settled(terminal.owner.registration)

    def test_output_refuses_before_buffer_growth(self):
        buffer = bytearray(b'x' * OUTPUT_BYTES)
        with self.assertRaises(ValueError):
            append_bounded(buffer, b'y')
        self.assertEqual(len(buffer), OUTPUT_BYTES)


if __name__ == '__main__':
    unittest.main()
