from __future__ import annotations

import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

REPO = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "m8_process_gate", REPO / "crates/rw-cli/tests/m8_release_gate.py"
)
assert SPEC is not None and SPEC.loader is not None
M8 = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(M8)


class ProcessImageTests(unittest.TestCase):
    def test_exact_private_copies_match_without_original_argv_path(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture = root / "approved-fixture"
            fixture.write_bytes(b"exact fixture image")
            images = {}
            for pid in (101, 102, 103):
                image = root / f"private-snapshot-{pid}"
                shutil.copyfile(fixture, image)
                images[pid] = image
            descendants = [(pid, 1, pid, "/unrelated/argv") for pid in images]
            with mock.patch.object(M8, "process_image_path", side_effect=images.__getitem__), \
                    mock.patch.object(M8.os, "getpgid", side_effect=lambda pid: pid):
                self.assertEqual(M8.fixture_processes(descendants, fixture),
                                 [(101, 101), (102, 102), (103, 103)])

    def test_original_argv_cannot_substitute_for_different_running_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture = root / "approved"
            fixture.write_bytes(b"approved bytes")
            other = root / "different"
            other.write_bytes(b"modified bytes")
            with mock.patch.object(M8, "process_image_path", return_value=other):
                self.assertEqual(M8.fixture_processes([(101, 1, 101, str(fixture))], fixture), [])

    def test_running_group_change_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = Path(directory) / "approved"
            fixture.write_bytes(b"image")
            with mock.patch.object(M8, "process_image_path", return_value=fixture), \
                    mock.patch.object(M8.os, "getpgid", return_value=202):
                with self.assertRaisesRegex(RuntimeError, "changed its process group"):
                    M8.fixture_processes([(101, 1, 101, "irrelevant")], fixture)

    def test_current_kernel_image_matches_without_argv_authority(self) -> None:
        pid, group = os.getpid(), os.getpgrp()
        self.assertEqual(
            M8.fixture_processes([(pid, os.getppid(), group, "spoofed-argv")],
                                 Path(sys.executable).resolve()),
            [(pid, group)],
        )

    def test_image_replacement_during_capture_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            first, second = Path(directory) / "first", Path(directory) / "second"
            first.write_bytes(b"same exact bytes")
            second.write_bytes(first.read_bytes())
            with mock.patch.object(M8, "process_image_path", side_effect=[first, second]):
                with self.assertRaisesRegex(RuntimeError, "changed its running image"):
                    M8.fixture_processes([(101, 1, 101, "irrelevant")], first)

    def test_unreadable_kernel_image_does_not_fall_back_to_argv(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = Path(directory) / "approved"
            fixture.write_bytes(b"image")
            with mock.patch.object(M8, "process_image_path", side_effect=PermissionError("denied")):
                with self.assertRaises(PermissionError):
                    M8.fixture_processes([(101, 1, 101, str(fixture))], fixture)

    @unittest.skipUnless(sys.platform == "linux", "Linux sealed executable kernel identity")
    def test_actual_sealed_memfd_exec_has_exact_identity_despite_spoofed_argv(self) -> None:
        import fcntl

        fixture = Path(shutil.which("sleep")).resolve()
        descriptor = os.memfd_create("m8-approved-fixture", os.MFD_ALLOW_SEALING)
        child = None
        try:
            with fixture.open("rb") as source:
                while chunk := source.read(64 * 1024):
                    view = memoryview(chunk)
                    while view:
                        view = view[os.write(descriptor, view):]
            os.fchmod(descriptor, 0o500)
            fcntl.fcntl(descriptor, fcntl.F_ADD_SEALS,
                        fcntl.F_SEAL_WRITE | fcntl.F_SEAL_GROW | fcntl.F_SEAL_SHRINK | fcntl.F_SEAL_SEAL)
            child = subprocess.Popen(["spoofed-argv", "30"],
                                     executable=f"/proc/self/fd/{descriptor}",
                                     pass_fds=(descriptor,), start_new_session=True)
            descendants = [(child.pid, os.getpid(), child.pid, "spoofed-argv 30")]
            self.assertEqual(M8.fixture_processes(descendants, fixture), [(child.pid, child.pid)])
        finally:
            if child is not None:
                child.kill()
                child.wait(timeout=5)
            os.close(descriptor)


if __name__ == "__main__":
    unittest.main()
