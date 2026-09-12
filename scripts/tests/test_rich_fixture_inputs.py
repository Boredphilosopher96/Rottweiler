"""Prepared-input identity rejects aliases, replacement and oversize files."""
import hashlib
import os
from pathlib import Path
import sys
import tempfile
import unittest
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from rich_fixture_inputs import file_identity


class RichInputsTests(unittest.TestCase):
    def test_exact_bytes_and_unowned_aliases(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            path = root / "plugin"
            path.write_bytes(b"fixture")
            self.assertEqual(file_identity(path, 7), {"bytes": 7, "sha256": hashlib.sha256(b"fixture").hexdigest()})
            with self.assertRaises(ValueError):
                file_identity(path, 6)
            alias = root / "alias"
            alias.symlink_to(path)
            with self.assertRaises(OSError):
                file_identity(alias, 7)
            alias.unlink()
            os.link(path, alias)
            with self.assertRaises(ValueError):
                file_identity(path, 7)

    def test_fifo_is_rejected_without_waiting_for_writer(self):
        with tempfile.TemporaryDirectory() as temporary:
            fifo = Path(temporary) / "fifo"
            os.mkfifo(fifo)
            with self.assertRaises(ValueError):
                file_identity(fifo, 7)


if __name__ == "__main__":
    unittest.main()
