"""Actual marker ordering, receive-clock bounds and incomplete recycle denial."""
import sys
from pathlib import Path
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from soak_outages import Outages, START, READY, INPUT


class OutageTests(unittest.TestCase):
    def marker(self, owner, marker, at, pid=10):
        return owner.feed(b'\n' + marker + b'\n', at, lambda: pid)

    def initial(self):
        owner = Outages(100)
        self.marker(owner, START, 100.1)
        self.marker(owner, READY, 100.3)
        self.marker(owner, INPUT, 100.4)
        return owner

    def test_forced_fault_has_distinct_restore_and_visible_input_intervals(self):
        owner = self.initial()
        owner.force(102, 102.001)
        self.marker(owner, START, 102.2)
        self.marker(owner, READY, 102.4, 11)
        self.marker(owner, INPUT, 102.8)
        owner.require_complete()
        record = owner.snapshot()['records'][1]
        self.assertEqual(record['pid'], 11)
        self.assertAlmostEqual(record['restoration_ms'], 200)
        self.assertAlmostEqual(record['forced_input_blackout_ms'], 800)
        self.assertAlmostEqual(record['ready_to_visible_input_ms'], 400)
        self.assertAlmostEqual(record['fault_signal_interval_ms'], 1)

    def test_natural_recycle_reports_bounds_not_invented_retirement_timestamp(self):
        owner = self.initial()
        owner.confirm_ready(101, 10)
        self.marker(owner, START, 102)
        self.marker(owner, READY, 102.3, 12)
        self.marker(owner, INPUT, 102.5)
        record = owner.snapshot()['records'][1]
        self.assertEqual(record['natural_blackout_observed_bounds_ms'], [500, 1500])
        self.assertNotIn('forced_input_blackout_ms', record)

    def test_fragmented_markers_count_once_without_accepting_large_line_suffix(self):
        owner = Outages(0)
        owner.feed(b'\nSOAK_TUI_PRO', .1, lambda: 1)
        owner.feed(b'CESS_START\n', .2, lambda: 1)
        owner.feed(b'x' * 10000 + READY, .3, lambda: 1)
        owner.feed(b'\n', .4, lambda: 1)
        self.assertNotIn('driver_ready_seconds', owner.generations[0])
        self.assertEqual(self.marker(owner, READY, .5), 1)
        self.assertLessEqual(len(owner.tail), len(START) + 1)

    def test_missing_ack_and_duplicate_or_unidentified_readiness_reject(self):
        owner = self.initial()
        owner.force(102, 102)
        self.marker(owner, START, 102.2)
        with self.assertRaisesRegex(ValueError, 'visible input'):
            owner.require_complete()
        with self.assertRaisesRegex(ValueError, 'identity'):
            self.marker(owner, READY, 102.3, None)
        self.marker(owner, READY, 102.3, 12)
        with self.assertRaisesRegex(ValueError, 'duplicate'):
            self.marker(owner, READY, 102.4, 12)


if __name__ == '__main__':
    unittest.main()
