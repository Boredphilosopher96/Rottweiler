from __future__ import annotations

import os
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import perf_process
from perf_process import run_sample


class PerformanceProcessTests(unittest.TestCase):
    def run_python(self, source, **options):
        return run_sample([sys.executable, "-c", source], cwd=Path.cwd(), env=dict(os.environ), **options)

    def test_drains_both_streams_without_waiting_for_one_to_finish(self):
        run = self.run_python("import os; os.write(2, b'e'*50000); os.write(1, b'o'*50000)")
        self.assertEqual(run.returncode, 0)
        self.assertEqual(run.stdout, b"o" * 50000)
        self.assertEqual(run.stderr, b"e" * 50000)

    def test_output_flood_is_bounded(self):
        with self.assertRaisesRegex(ValueError, "output bytes"):
            self.run_python("import os\nwhile True: os.write(1, b'x'*4096)", output_limit=8192)

    def test_timeout_reaps_the_child_before_returning(self):
        with tempfile.TemporaryDirectory() as temporary:
            pid_file = Path(temporary) / "pid"
            source = f"import os,time; open({str(pid_file)!r},'w').write(str(os.getpid())); time.sleep(60)"
            with self.assertRaises(TimeoutError):
                self.run_python(source, timeout=.5)
            pid = int(pid_file.read_text())
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)

    def test_exited_leader_cannot_leave_a_pipe_holding_descendant_running(self):
        with tempfile.TemporaryDirectory() as temporary:
            effect = Path(temporary) / "effect"
            source = f"""import os,time
if os.fork() == 0:
    time.sleep(1)
    open({str(effect)!r}, 'w').write('escaped')
else:
    os._exit(0)
"""
            with self.assertRaises(TimeoutError):
                self.run_python(source, timeout=.3)
            time.sleep(1)
            self.assertFalse(effect.exists())

    def test_closed_pipes_do_not_bypass_the_process_deadline(self):
        with self.assertRaises(TimeoutError):
            self.run_python("import os,time; os.close(1); os.close(2); time.sleep(60)", timeout=.3)

    def test_nonblocking_eof_check_does_not_depend_on_readiness_notifications(self):
        class DelayedNotifications(perf_process.selectors.DefaultSelector):
            def select(self, timeout=None):
                time.sleep(min(timeout or .002, .002))
                return []

        with patch.object(perf_process.selectors, "DefaultSelector", DelayedNotifications):
            result = self.run_python("import os; os.write(1,b'ready'); os.write(2,b'marker')")
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, b"ready")
        self.assertEqual(result.stderr, b"marker")

    def test_timeout_diagnostics_identify_process_and_pipe_state_without_payloads(self):
        with self.assertRaisesRegex(TimeoutError, r"leader=running, pending_pipes=2, stdout_bytes=6") as failure:
            self.run_python("import os,time; os.write(1,b'secret'); time.sleep(60)", timeout=.5)
        self.assertNotIn("secret", str(failure.exception))


if __name__ == "__main__":
    unittest.main()
