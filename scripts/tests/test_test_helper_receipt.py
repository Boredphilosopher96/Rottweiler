"""Native fixture publication binds both Cargo artifacts in one complete bundle."""
from concurrent.futures import ThreadPoolExecutor
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import threading
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location(
    "build_test_helper", Path(__file__).resolve().parents[1] / "build-test-helper.py")
assert SPEC is not None and SPEC.loader is not None
HELPER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HELPER)


def artifacts(root):
    paths = [root / name for name in HELPER.BINARIES]
    for index, path in enumerate(paths):
        path.write_bytes(f"trusted build artifact {index}".encode())
        path.chmod(0o700)
    return paths


class HelperReceiptTests(unittest.TestCase):
    def test_receipt_owns_exact_artifact_identity_and_both_hashes(self):
        with tempfile.TemporaryDirectory() as directory:
            executable, fixture = artifacts(Path(directory))
            receipt = HELPER.write_receipt(executable, fixture)
            body = json.loads(receipt.read_text())
            snapshot = Path(body["executable"])
            self.assertNotEqual(snapshot, executable.resolve())
            metadata = snapshot.stat()
            self.assertEqual(body, {
                "executable": str(snapshot), "device": metadata.st_dev,
                "inode": metadata.st_ino, "bytes": metadata.st_size,
                "sha256": hashlib.sha256(executable.read_bytes()).hexdigest(),
            })
            sibling = receipt.parent / (HELPER.FIXTURE + ".identity.json")
            peer = json.loads(sibling.read_text())
            self.assertEqual(receipt.parent.name, body["sha256"] + "-" + peer["sha256"])
            self.assertEqual(HELPER.write_receipt(executable, fixture), receipt)
            fixture.write_bytes(b"changed fixture")
            changed_fixture = HELPER.write_receipt(executable, fixture)
            self.assertNotEqual(changed_fixture, receipt)
            self.assertEqual(Path(peer["executable"]).read_bytes(), b"trusted build artifact 1")
            executable.write_bytes(b"changed helper")
            self.assertNotEqual(HELPER.write_receipt(executable, fixture), changed_fixture)
            self.assertEqual(snapshot.read_bytes(), b"trusted build artifact 0")
            self.assertEqual(snapshot.stat().st_ino, body["inode"])
            self.assertEqual(HELPER.ENVIRONMENT_KEY, "ROTTWEILER_TEST_SANDBOX_HELPER_RECEIPT")

    def test_publication_rename_exposes_all_members_and_receipts_together(self):
        with tempfile.TemporaryDirectory() as directory:
            executable, fixture = artifacts(Path(directory))
            rename = Path.rename
            observed = []

            def publish(staging, destination):
                self.assertFalse(destination.exists())
                self.assertEqual({path.name for path in staging.iterdir()}, {
                    HELPER.BINARY, HELPER.FIXTURE,
                    HELPER.BINARY + ".identity.json", HELPER.FIXTURE + ".identity.json"})
                for name in HELPER.BINARIES:
                    body = json.loads((staging / (name + ".identity.json")).read_text())
                    self.assertEqual(body["executable"], str(destination / name))
                    self.assertEqual(body["inode"], (staging / name).stat().st_ino)
                observed.append(destination)
                return rename(staging, destination)

            with mock.patch.object(Path, "rename", publish):
                receipt = HELPER.write_receipt(executable, fixture)
            self.assertEqual(observed, [receipt.parent])

    def test_concurrent_publishers_return_the_same_complete_bundle(self):
        with tempfile.TemporaryDirectory() as directory:
            executable, fixture = artifacts(Path(directory))
            barrier = threading.Barrier(2)
            rename = Path.rename

            def publish(staging, destination):
                barrier.wait(timeout=5)
                return rename(staging, destination)

            with mock.patch.object(Path, "rename", publish), ThreadPoolExecutor(max_workers=2) as workers:
                futures = [workers.submit(HELPER.write_receipt, executable, fixture) for _ in range(2)]
                receipts = [future.result(timeout=5) for future in futures]
            self.assertEqual(receipts[0], receipts[1])
            self.assertEqual(len(list(receipts[0].parent.iterdir())), 4)
            self.assertFalse(any(p.name.startswith(".building-") for p in receipts[0].parent.parent.iterdir()))

    def test_failed_second_copy_never_publishes_a_partial_bundle(self):
        with tempfile.TemporaryDirectory() as directory:
            executable, fixture = artifacts(Path(directory))
            fixture.write_bytes(b"")
            with self.assertRaisesRegex(RuntimeError, "size or mode"):
                HELPER.write_receipt(executable, fixture)
            self.assertEqual(list((executable.parent / ".rw-test-helpers").iterdir()), [])

    def test_incomplete_or_corrupt_existing_bundle_is_rejected_without_repair(self):
        for corruption in ("missing", "receipt", "bytes", "symlink"):
            with self.subTest(corruption=corruption), tempfile.TemporaryDirectory() as directory:
                executable, fixture = artifacts(Path(directory))
                receipt = HELPER.write_receipt(executable, fixture)
                sibling = receipt.parent / (HELPER.FIXTURE + ".identity.json")
                if corruption == "missing":
                    sibling.unlink()
                elif corruption == "receipt":
                    sibling.write_text("{}\n")
                elif corruption == "symlink":
                    sibling.unlink()
                    sibling.symlink_to(receipt)
                else:
                    snapshot = receipt.parent / HELPER.FIXTURE
                    snapshot.chmod(0o700)
                    snapshot.write_bytes(b"substituted")
                    snapshot.chmod(0o500)
                with self.assertRaisesRegex(RuntimeError, "bundle|identity|regular|receipt"):
                    HELPER.write_receipt(executable, fixture)
                if corruption == "missing":
                    self.assertFalse(sibling.exists())
                self.assertFalse(any(p.name.startswith(".building-") for p in receipt.parent.parent.iterdir()))

    def test_special_published_member_is_rejected_without_blocking(self):
        with tempfile.TemporaryDirectory() as directory:
            executable, fixture = artifacts(Path(directory))
            receipt = HELPER.write_receipt(executable, fixture)
            snapshot = receipt.parent / HELPER.FIXTURE
            snapshot.unlink()
            os.mkfifo(snapshot)
            with self.assertRaisesRegex(RuntimeError, "regular"):
                HELPER.write_receipt(executable, fixture)

    def test_empty_or_nonexecutable_artifact_is_rejected(self):
        for member in range(2):
            with tempfile.TemporaryDirectory() as directory:
                inputs = artifacts(Path(directory))
                inputs[member].write_bytes(b"")
                with self.assertRaisesRegex(RuntimeError, "size or mode"):
                    HELPER.write_receipt(*inputs)
                inputs[member].write_bytes(b"code")
                inputs[member].chmod(0o600)
                with self.assertRaisesRegex(RuntimeError, "size or mode"):
                    HELPER.write_receipt(*inputs)


if __name__ == "__main__":
    unittest.main()
