"""One physical retirement envelope covers nested daemon cleanup and proof."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import Mock, patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import perf_process_owner as owners
from perf_process_scope import UnsettledScope


class RetirementTests(unittest.TestCase):
    def test_force_reap_and_absence_share_one_deadline(self):
        clock = [0.0]
        owner = owners.OwnedProcess.__new__(owners.OwnedProcess)
        owner.finished, owner.failure, owner.scope = False, None, None
        owner.registration = 'fixture'
        owner.process = Mock(pid=123, stdout=None, stderr=None)
        def force(*_args, **_kwargs):
            clock[0] += 1
        def reap(*, timeout):
            self.assertEqual(timeout, 14)
            clock[0] += 4
        def absence(_pid, *, timeout):
            self.assertEqual(timeout, 10)
            clock[0] += timeout
        owner.process.wait.side_effect = reap
        with patch.object(owners.time, 'monotonic', side_effect=lambda: clock[0]), \
                patch.object(owners, 'signal_owned_group', side_effect=force), \
                patch.object(owners, 'require_group_disappearance', side_effect=absence), \
                patch.object(owners, 'SCOPE'):
            owner.settle()
        self.assertEqual(clock[0], 15)

    def test_ack_after_cooperative_deadline_cannot_qualify(self):
        clock = [0.0]
        owner = owners.OwnedProcess.__new__(owners.OwnedProcess)
        owner.finished, owner.failure, owner.registration = False, None, 'fixture'
        owner.process = Mock(pid=123, stdout=None, stderr=None)
        owner.scope = Mock(descriptor=999, closed=False)
        def sleep(seconds):
            clock[0] += seconds
        def force(_pid, number, **_kwargs):
            if number == signal.SIGKILL:
                owner.scope.closed = True
        with patch.object(owners.time, 'monotonic', side_effect=lambda: clock[0]), \
                patch.object(owners.time, 'sleep', side_effect=sleep), \
                patch.object(owners, 'signal_owned_group', side_effect=force), \
                patch.object(owners, 'observe_exit', return_value=0), \
                patch.object(owners, 'require_group_disappearance'), \
                patch.object(owners.os, 'close'), patch.object(owners, 'SCOPE') as scope:
            with self.assertRaisesRegex(UnsettledScope, 'cooperative retirement deadline'):
                owner.settle()
            scope.settled.assert_not_called()

    def test_expired_daemon_budget_refuses_new_cleanup_process(self):
        import m8_container
        with patch.object(m8_container, 'OwnedProcess') as launch:
            with self.assertRaisesRegex(UnsettledScope, 'expired before launch'):
                m8_container.control(['docker', 'rm', '--force', 'a' * 64],
                                     'active', cleanup=True, deadline=time.monotonic() - 1)
            launch.assert_not_called()

    def test_slow_m8_cancel_fits_outer_grace_and_stuck_cleanup_fails(self):
        for stuck in (False, True):
            with self.subTest(stuck=stuck), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                docker = root / 'docker'
                docker.write_text('#!' + sys.executable + '\n' +
                    'import os,pathlib,sys,time\nargs=sys.argv[1:]\n' +
                    f'root=pathlib.Path({str(root)!r})\nstuck={stuck!r}\n' +
                    "if args[0]=='create':\n"
                    " (root/'creating').write_text('ready')\n"
                    " time.sleep(.1 if stuck else 3.2)\n"
                    " print('a'*64)\n"
                    "elif args[0]=='rm':\n"
                    " time.sleep(60 if stuck else 1.6)\n"
                    " (root/'removed').write_text('done')\n"
                    "elif args[:2]==['container','ls']:\n"
                    " time.sleep(1.6)\n"
                    " (root/'absence').write_text('done')\n")
                docker.chmod(0o700)
                output = root / 'result.json'
                process = subprocess.Popen([sys.executable, str(SCRIPTS/'ci_evidence.py'),
                    '--delegated', '--gate', 'm8-cancel', '--output', str(output), '--',
                    sys.executable, str(SCRIPTS/'m8_container.py'), 'rottweiler-m8-1-2', '--',
                    'docker', 'run', '--rm', '--name', 'rottweiler-m8-1-2', 'fixture'],
                    env=dict(os.environ, PATH=str(root)+os.pathsep+os.environ['PATH']),
                    stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                try:
                    ready_by = time.monotonic()+3
                    while not (root/'creating').exists():
                        if time.monotonic() >= ready_by:
                            self.fail('Docker creation not reached')
                        time.sleep(.01)
                    started = time.monotonic()
                    process.send_signal(signal.SIGTERM)
                    _, error = process.communicate(timeout=17)
                    elapsed = time.monotonic()-started
                    evidence = json.loads(output.read_text())
                    self.assertEqual(process.returncode, 130, error.decode())
                    self.assertLess(elapsed, 16)
                    if stuck:
                        self.assertIn('UNSETTLED', evidence['cleanup_error'])
                        self.assertFalse((root/'removed').exists())
                        self.assertFalse((root/'absence').exists())
                    else:
                        self.assertGreater(elapsed, 5)
                        self.assertNotIn('cleanup_error', evidence)
                        self.assertTrue((root/'removed').exists())
                        self.assertTrue((root/'absence').exists())
                finally:
                    if process.poll() is None:
                        process.kill()
                        process.wait(timeout=3)
