"""Prepared-input identity rejects aliases, replacement and oversize files."""
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import rich_fixture_inputs
from rich_fixture_inputs import FILES, file_identity, file_snapshot, verify


class RichInputsTests(unittest.TestCase):
    def test_exact_bytes_and_unowned_aliases(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
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

    def test_replacement_during_read_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "plugin"
            replacement = Path(temporary) / "replacement"
            path.write_bytes(b"fixture")
            replacement.write_bytes(b"changed")
            real_readv = os.readv
            replaced = False

            def replacing_read(descriptor, buffers):
                nonlocal replaced
                count = real_readv(descriptor, buffers)
                if count and not replaced:
                    os.replace(replacement, path)
                    replaced = True
                return count

            with mock.patch.object(rich_fixture_inputs.os, "readv", side_effect=replacing_read):
                with self.assertRaisesRegex(ValueError, "changed during identity capture"):
                    file_snapshot(path, 7)

    def test_growth_during_read_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "plugin"
            path.write_bytes(b"fixture")
            real_readv = os.readv
            grown = False

            def growing_read(descriptor, buffers):
                nonlocal grown
                count = real_readv(descriptor, buffers)
                if count and not grown:
                    with path.open("ab") as stream:
                        stream.write(b"!")
                    grown = True
                return count

            with mock.patch.object(rich_fixture_inputs.os, "readv", side_effect=growing_read):
                with self.assertRaisesRegex(ValueError, "grew beyond its byte bound"):
                    file_snapshot(path, 7)

    def test_verify_parses_the_receipt_snapshot_without_reopening_it(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            repo, candidate, prepared = root / "repo", root / "candidate", root / "prepared"
            repo.mkdir()
            candidate.mkdir()
            prepared.mkdir()
            (repo / ".bun-version").write_text("1.3.14\n")
            bun = root / "bun"
            bun.write_bytes(b"pinned bun")
            bun.chmod(0o700)
            for name in FILES:
                (prepared / name).write_bytes(name.encode())
            product = {"identity_sha256": "candidate-identity", "identity": {"source": "source-identity"}}
            receipt = {
                "schema_version": 1,
                "candidate_identity": product["identity_sha256"],
                "source": product["identity"]["source"],
                "bun": {"path": str(bun), "version": "1.3.14", **file_identity(bun, 256 * 1024 * 1024)},
                "files": {name: file_identity(prepared / name, limit) for name, limit in FILES.items()},
            }
            receipt_path = prepared / "rich-fixture.json"
            receipt_path.write_text(json.dumps(receipt))
            with mock.patch.object(rich_fixture_inputs.native_candidate, "verify", return_value=product), \
                    mock.patch.object(Path, "read_bytes", side_effect=AssertionError("receipt reopened")):
                observed = verify(candidate, receipt_path, repo)
            self.assertEqual(observed["prepared"], receipt)
            self.assertEqual(observed["receipt_sha256"], hashlib.sha256(receipt_path.read_bytes()).hexdigest())


if __name__ == "__main__":
    unittest.main()
