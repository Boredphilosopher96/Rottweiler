"""The native fixture prerequisite binds exactly the Cargo-owned artifact bytes."""
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location(
    "build_test_helper", Path(__file__).resolve().parents[1] / "build-test-helper.py")
assert SPEC is not None and SPEC.loader is not None
HELPER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HELPER)


class HelperReceiptTests(unittest.TestCase):
    def test_receipt_owns_exact_artifact_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / "rw-sandbox-helper"
            executable.write_bytes(b"trusted build artifact")
            executable.chmod(0o700)
            receipt = HELPER.write_receipt(executable)
            body = json.loads(receipt.read_text())
            metadata = executable.stat()
            self.assertEqual(body, {
                "executable": str(executable.resolve()), "device": metadata.st_dev,
                "inode": metadata.st_ino, "bytes": metadata.st_size,
                "sha256": hashlib.sha256(executable.read_bytes()).hexdigest(),
            })
            executable.write_bytes(b"changed artifact")
            self.assertNotEqual(body["sha256"], hashlib.sha256(executable.read_bytes()).hexdigest())
            self.assertEqual(HELPER.ENVIRONMENT_KEY, "ROTTWEILER_TEST_SANDBOX_HELPER_RECEIPT")

    def test_empty_or_nonexecutable_artifact_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / "rw-sandbox-helper"
            executable.touch(mode=0o700)
            with self.assertRaisesRegex(RuntimeError, "size or mode"):
                HELPER.write_receipt(executable)
            executable.write_bytes(b"code")
            executable.chmod(0o600)
            with self.assertRaisesRegex(RuntimeError, "size or mode"):
                HELPER.write_receipt(executable)


if __name__ == "__main__":
    unittest.main()
