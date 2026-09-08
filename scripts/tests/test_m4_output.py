import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from m4_output import EngineErrorLog, MAX_STDERR_BYTES
from perf_process_scope import UnsettledScope


class M4OutputTests(unittest.TestCase):
    def test_stderr_flood_is_drained_but_retained_bytes_are_bounded(self):
        with tempfile.TemporaryDirectory() as directory:
            output = EngineErrorLog(Path(directory) / 'stderr')
            child = subprocess.Popen([sys.executable, '-c',
                                      "import os; [os.write(2,b'x'*65536) for _ in range(64)]"],
                                     stdout=subprocess.DEVNULL, stderr=output.write_fd)
            output.close_input()
            try:
                self.assertEqual(child.wait(timeout=5), 0)
                with self.assertRaisesRegex(ValueError, 'stderr exceeded'):
                    output.finish()
                self.assertEqual(output.path.stat().st_size, MAX_STDERR_BYTES)
                self.assertFalse(output.worker.is_alive())
            finally:
                if child.returncode is None:
                    child.kill()
                    child.wait()

    def test_unknown_writer_cannot_be_reported_as_finished(self):
        with tempfile.TemporaryDirectory() as directory:
            output = EngineErrorLog(Path(directory) / 'stderr')
            retained_writer = os.dup(output.write_fd)
            try:
                with self.assertRaises(UnsettledScope):
                    output.finish()
                self.assertFalse(output.worker.is_alive())
            finally:
                os.close(retained_writer)

    def test_empty_settled_stderr_closes_both_pipe_and_file(self):
        with tempfile.TemporaryDirectory() as directory:
            output = EngineErrorLog(Path(directory) / 'stderr')
            output.finish()
            self.assertTrue(output.eof)
            self.assertTrue(output.output.closed)
            self.assertEqual(output.path.stat().st_size, 0)


if __name__ == '__main__':
    unittest.main()
