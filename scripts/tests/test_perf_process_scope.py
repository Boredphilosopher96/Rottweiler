from __future__ import annotations

import json
import os
from pathlib import Path
import signal
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import perf_process
from perf_process_scope import ScopeReader, UnsettledScope


class NestedPerformanceProcessTests(unittest.TestCase):
    def wrapper(self, source, **options):
        env = dict(os.environ, PYTHONPATH=str(Path(perf_process.__file__).parent))
        return perf_process.run_sample([sys.executable, "-c", source], cwd=Path.cwd(), env=env, delegated=True, **options)

    def test_success_requires_closed_scope_and_does_not_pass_capability_to_native_child(self):
        source = """import os,sys
from pathlib import Path
from perf_process import run_sample
run = run_sample([sys.executable,'-c',"import os; assert 'RW_PERF_SETTLEMENT_FD' not in os.environ; print('ready')"],cwd=Path.cwd(),env=dict(os.environ))
assert run.stdout == b'ready\\n'
"""
        self.assertEqual(self.wrapper(source).returncode, 0)
        with self.assertRaisesRegex(UnsettledScope, "missing physical-settlement"):
            self.wrapper("print('no owner acknowledgement')")

    def test_soft_cancellation_reaps_nested_child_before_scope_acknowledgement(self):
        with tempfile.TemporaryDirectory() as temporary:
            pid_path = Path(temporary) / "child.pid"
            child = f"import os,time; open({str(pid_path)!r},'w').write(str(os.getpid())); time.sleep(60)"
            wrapper = f"""import os,sys
from pathlib import Path
from perf_process import run_sample
run_sample([sys.executable,'-c',{child!r}],cwd=Path.cwd(),env=dict(os.environ),timeout=60)
"""
            with self.assertRaises(TimeoutError) as failed:
                self.wrapper(wrapper, timeout=.4)
            self.assertNotIsInstance(failed.exception, UnsettledScope)
            pid = int(pid_path.read_text())
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)

    @unittest.skipUnless(hasattr(os, "fork"), "requires a same-group nested owner")
    def test_exited_wrapper_keeps_nested_owner_alive_for_cooperative_settlement(self):
        with tempfile.TemporaryDirectory() as temporary:
            pid_path = Path(temporary) / "child.pid"
            native = f"import os,time; open({str(pid_path)!r},'w').write(str(os.getpid())); time.sleep(60)"
            wrapper = f"""import os,sys,time
from pathlib import Path
from perf_process import run_sample
if os.fork():
    while not Path({str(pid_path)!r}).exists(): time.sleep(.01)
    os._exit(0)
run_sample([sys.executable,'-c',{native!r}],cwd=Path.cwd(),env=dict(os.environ),timeout=60)
"""
            with self.assertRaises(TimeoutError) as failed:
                self.wrapper(wrapper, timeout=.5)
            self.assertNotIsInstance(failed.exception, UnsettledScope)
            with self.assertRaises(ProcessLookupError):
                os.kill(int(pid_path.read_text()), 0)

    def test_forced_wrapper_death_during_start_and_after_announcement_is_unsettled(self):
        for announced in (False, True):
            with self.subTest(announced=announced), tempfile.TemporaryDirectory() as temporary:
                pid_path = Path(temporary) / "child.pid"
                wrapper = f"""import os,signal,subprocess,sys
import perf_process
scope = perf_process._SCOPE
token = scope.starting()
child = subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'], start_new_session=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
open({str(pid_path)!r},'w').write(str(child.pid))
if {announced!r}: scope.started(token, child.pid)
os.kill(os.getpid(), signal.SIGKILL)
"""
                try:
                    with self.assertRaisesRegex(UnsettledScope, "UNSETTLED") as failure:
                        self.wrapper(wrapper)
                    self.assertIn("active_children", str(failure.exception))
                    pid = int(pid_path.read_text())
                    # The supervisor makes no false orphan-cleanup claim. This
                    # fixture explicitly owns investigation/cleanup after failure.
                    os.kill(pid, 0)
                finally:
                    if pid_path.exists():
                        try:
                            os.killpg(int(pid_path.read_text()), signal.SIGKILL)
                        except ProcessLookupError:
                            pass

    def test_supervisor_never_signals_a_delegated_or_reused_pid(self):
        wrapper = """import os
import perf_process
scope = perf_process._SCOPE
token = scope.starting(); scope.started(token, 1)
os._exit(0)
"""
        with patch.object(perf_process.os, "killpg", wraps=os.killpg) as kill:
            with self.assertRaises(UnsettledScope):
                self.wrapper(wrapper)
            self.assertTrue(kill.called)
            self.assertTrue(all(call.args[0] != 1 for call in kill.call_args_list))

    def test_cancelled_scope_allows_only_cleanup_of_an_existing_owner(self):
        from perf_process_owner import OwnedProcess, SCOPE
        from perf_process_scope import ScopeCancelled
        parent = SCOPE.starting()
        previous = SCOPE.cancelled
        SCOPE.cancelled = signal.SIGTERM
        try:
            with self.assertRaises(ScopeCancelled):
                OwnedProcess([sys.executable, "-c", "pass"], cwd=Path.cwd(), env=dict(os.environ))
            with self.assertRaisesRegex(RuntimeError, "active physical owner"):
                OwnedProcess([sys.executable, "-c", "pass"], cwd=Path.cwd(), env=dict(os.environ), cleanup_of="unowned")
            owner = OwnedProcess([sys.executable, "-c", "print('cleanup')"],
                                 cwd=Path.cwd(), env=dict(os.environ), cleanup_of=parent)
            try:
                import time
                while owner.observe_exit() is None:
                    time.sleep(.001)
                self.assertEqual(owner.process.stdout.read(), b"cleanup\n")
            finally:
                owner.settle()
            SCOPE.settled(parent)
        finally:
            SCOPE.cancelled = previous

    def test_stale_settlement_cannot_release_another_owner_with_the_same_pid(self):
        reader = ScopeReader(-1)
        for message in [
            {"kind": "starting", "token": "old"}, {"kind": "started", "token": "old", "pid": 7},
            {"kind": "settled", "token": "old"}, {"kind": "starting", "token": "new"},
            {"kind": "started", "token": "new", "pid": 7}, {"kind": "settled", "token": "old"},
        ]:
            reader.append(json.dumps(message).encode() + b"\n")
        self.assertEqual(reader.active, {"new": 7})
        self.assertIsNotNone(reader.failure)


if __name__ == "__main__":
    unittest.main()
