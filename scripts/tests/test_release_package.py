from __future__ import annotations

import gzip
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[2]
PACKAGER = REPO / "scripts" / "package-release.py"


class ReleasePackageTests(unittest.TestCase):
    def make_stage(self, root: Path, timestamp: int) -> Path:
        stage = root / "rottweiler-1.2.3-linux-x86_64"
        binary = stage / "bin" / "rw"
        binary.parent.mkdir(parents=True)
        binary.write_bytes(b"deterministic engine\n")
        binary.chmod(0o755)
        notice = stage / "NOTICE"
        notice.write_text("same release\n", encoding="utf-8")
        for path in (stage, binary.parent, binary, notice):
            path.chmod(0o755 if path.is_dir() or path == binary else 0o644)
        # Deliberately make checkout/build timestamps differ.
        for path in (stage, binary.parent, binary, notice):
            os.utime(path, (timestamp, timestamp))
        return stage

    def run_packager(self, stage: Path, archive: Path) -> None:
        subprocess.run(
            [sys.executable, str(PACKAGER), str(stage), str(archive), "--epoch", "1234567890"],
            check=True,
        )

    def test_identical_trees_from_different_roots_are_byte_identical(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            first = self.make_stage(root / "a", 1_600_000_000)
            second = self.make_stage(root / "b", 1_700_000_000)
            first_archive = root / "first.tar.gz"
            second_archive = root / "second.tar.gz"
            self.run_packager(first, first_archive)
            self.run_packager(second, second_archive)
            self.assertEqual(
                hashlib.sha256(first_archive.read_bytes()).digest(),
                hashlib.sha256(second_archive.read_bytes()).digest(),
            )

    def test_archive_has_canonical_metadata_and_no_source_paths(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stage = self.make_stage(root / "checkout", 1_700_000_000)
            archive = root / "release.tar.gz"
            self.run_packager(stage, archive)

            with gzip.open(archive, "rb") as stream, tarfile.open(fileobj=stream, mode="r:") as tar:
                members = tar.getmembers()
            self.assertEqual([member.name for member in members], sorted(member.name for member in members))
            self.assertTrue(all(member.mtime == 1_234_567_890 for member in members))
            self.assertTrue(all(member.uid == 0 and member.gid == 0 for member in members))
            self.assertTrue(all(str(root) not in member.name for member in members))
            rw = next(member for member in members if member.name.endswith("/bin/rw"))
            self.assertEqual(rw.mode, 0o755)

    def test_non_regular_staging_entries_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stage = self.make_stage(root / "checkout", 1_700_000_000)
            (stage / "unsafe-link").symlink_to(stage / "NOTICE")
            run = subprocess.run(
                [sys.executable, str(PACKAGER), str(stage), str(root / "release.tar.gz")],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertNotEqual(run.returncode, 0)
            self.assertFalse((root / "release.tar.gz").exists())


if __name__ == "__main__":
    unittest.main()
