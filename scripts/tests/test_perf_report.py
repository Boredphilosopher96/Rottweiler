import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import perf_report


class PerformanceReportTests(unittest.TestCase):
    def test_oversized_report_is_rejected_before_decoding(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'report.json'
            with path.open('wb') as output:
                output.truncate(perf_report.MEMORY_REPORT_BYTES + 1)
            with patch.object(perf_report.json, 'loads', side_effect=AssertionError('decoded before admission')):
                with self.assertRaisesRegex(ValueError, 'bounded file'):
                    perf_report.read_report(path, perf_report.MEMORY_REPORT_BYTES)

    def test_report_symlinks_are_not_read_and_valid_object_is_accepted(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'report.json'
            path.write_text('{"schemaVersion":1}')
            self.assertEqual(perf_report.read_report(path, 128), {'schemaVersion': 1})
            alias = Path(directory) / 'alias.json'
            alias.symlink_to(path)
            with self.assertRaises(OSError):
                perf_report.read_report(alias, 128)


if __name__ == '__main__':
    unittest.main()
