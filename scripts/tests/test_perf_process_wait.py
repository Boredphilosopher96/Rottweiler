from __future__ import annotations

import os
from pathlib import Path
import signal
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import perf_process_wait as wait


class ProcessExitHandoffTests(unittest.TestCase):
    def test_darwin_denied_signal_waits_for_exit_without_releasing_identity(self):
        with patch.object(wait.sys, 'platform', 'darwin'), \
                patch.object(os, 'killpg', side_effect=PermissionError('exiting')) as send, \
                patch.object(wait, 'observe_exit', side_effect=[None, None, 7]) as observe, \
                patch.object(os, 'waitpid', side_effect=AssertionError('premature reap')), \
                patch.object(wait.time, 'sleep'):
            wait.signal_owned_group(123, signal.SIGKILL)
        send.assert_called_once_with(123, signal.SIGKILL)
        self.assertEqual(observe.call_count, 3)

    def test_live_permission_denial_is_not_an_exit_handoff(self):
        with patch.object(wait.sys, 'platform', 'darwin'), \
                patch.object(os, 'killpg', side_effect=PermissionError('denied')), \
                patch.object(wait, 'observe_exit', return_value=None), \
                patch.object(wait.time, 'monotonic', side_effect=[0, 2]):
            with self.assertRaisesRegex(PermissionError, 'denied'):
                wait.signal_owned_group(123, signal.SIGKILL)

    def test_exited_leader_cannot_prove_live_or_denied_group_disappearance(self):
        from perf_process_scope import UnsettledScope
        with patch.object(os, 'killpg', side_effect=PermissionError('group remains')) as probe:
            with self.assertRaises(UnsettledScope):
                wait.require_group_disappearance(123, timeout=0)
        probe.assert_called_once_with(123, 0)

    def test_non_darwin_permission_denial_is_reported_directly(self):
        with patch.object(wait.sys, 'platform', 'linux'), \
                patch.object(os, 'killpg', side_effect=PermissionError('denied')), \
                patch.object(wait, 'observe_exit', side_effect=AssertionError('unexpected wait')):
            with self.assertRaisesRegex(PermissionError, 'denied'):
                wait.signal_owned_group(123, signal.SIGKILL)
