from pathlib import Path
import shutil
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from perf_process_scope import UnsettledScope
from perf_scratch import retained_scratch


class ScratchTests(unittest.TestCase):
    def test_unproven_settlement_preserves_storage_and_original_failure(self):
        with tempfile.TemporaryDirectory() as parent:
            retained = []
            with self.assertRaisesRegex(UnsettledScope, "unacknowledged child"):
                with retained_scratch("sample-", parent=Path(parent), evidence=retained.append) as root:
                    (root / "native-owner").write_text("pending physical settlement")
                    raise UnsettledScope("unacknowledged child")
            self.assertEqual(retained, [root])
            self.assertEqual((root / "native-owner").read_text(), "pending physical settlement")
            shutil.rmtree(root)

    def test_successful_scope_removes_private_storage(self):
        with tempfile.TemporaryDirectory() as parent:
            retained = []
            with retained_scratch("sample-", parent=Path(parent), evidence=retained.append) as root:
                self.assertTrue(root.is_dir())
            self.assertFalse(root.exists())
            self.assertEqual(retained, [])


if __name__ == "__main__":
    unittest.main()
