from __future__ import annotations

import os
from pathlib import Path
import platform
import subprocess
import tempfile
import unittest

from scripts.release_contract import load_contract, render_installer


REPO = Path(__file__).resolve().parents[2]
TEMPLATE = REPO / "scripts" / "install-release.sh"
VERSION = "1.2.3"
CONTRACT = load_contract()
PLATFORM_CONTRACT = CONTRACT.resolve_platform(platform.system(), platform.machine())
PLATFORM = PLATFORM_CONTRACT.id


class ReleaseInstallTests(unittest.TestCase):
    def make_release(self, root: Path) -> Path:
        release = root / f"rottweiler-{VERSION}-{PLATFORM}"
        binary_dir = release / "bin"
        binary_dir.mkdir(parents=True)
        installer = render_installer(CONTRACT, TEMPLATE, VERSION, PLATFORM)
        (release / "install.sh").write_text(installer, encoding="utf-8")
        rw = binary_dir / "rw"
        rw.write_text(
            "#!/bin/sh\n"
            "case \"${1:-}\" in\n"
            "  --version) printf 'rw 1.2.3\\n' ;;\n"
            "  __install-sync) printf '%s\\n' \"$*\" >> \"${ROTTWEILER_INSTALL_SYNC_LOG:-/dev/null}\" ;;\n"
            "  *) exit 1 ;;\n"
            "esac\n",
            encoding="utf-8",
        )
        tui = binary_dir / "rottweiler-tui"
        tui.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        wasm_host = binary_dir / "rottweiler-wasm-host"
        wasm_host.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        native = binary_dir / PLATFORM_CONTRACT.native_library
        native.write_bytes(b"native fixture\n")
        (release / "install.sh").chmod(0o755)
        rw.chmod(0o755)
        tui.chmod(0o755)
        wasm_host.chmod(0o755)
        return release

    def install(self, release: Path, prefix: Path) -> subprocess.CompletedProcess[bytes]:
        environment = os.environ.copy()
        environment["ROTTWEILER_INSTALL_SYNC_LOG"] = str(prefix.parent / "install-sync.log")
        return subprocess.run(
            [str(release / "install.sh"), "--prefix", str(prefix)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=environment,
            check=False,
        )

    def test_installs_idempotently_with_atomic_managed_selectors(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release = self.make_release(root)
            prefix = root / "installed"
            first = self.install(release, prefix)
            self.assertEqual(first.returncode, 0, first.stderr.decode())
            second = self.install(release, prefix)
            self.assertEqual(second.returncode, 0, second.stderr.decode())
            self.assertEqual(os.readlink(prefix / "current"), f"versions/{VERSION}")
            self.assertEqual(os.readlink(prefix / "bin" / "rw"), "../current/bin/rw")
            self.assertTrue((prefix / "versions" / VERSION / "install.sh").is_file())
            sync_log = (root / "install-sync.log").read_text(encoding="utf-8")
            self.assertIn("__install-sync", sync_log)
            self.assertIn(str(prefix / "versions"), sync_log)
            self.assertIn(str(prefix / "bin"), sync_log)
            run = subprocess.run(
                [str(prefix / "bin" / "rw"), "--version"],
                stdout=subprocess.PIPE,
                check=True,
            )
            self.assertEqual(run.stdout, b"rw 1.2.3\n")

    def test_refuses_unexpected_archive_entries_before_mutation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release = self.make_release(root)
            (release / "secret").write_text("unexpected", encoding="utf-8")
            prefix = root / "installed"
            run = self.install(release, prefix)
            self.assertNotEqual(run.returncode, 0)
            self.assertFalse(prefix.exists())

    def test_refuses_to_replace_a_different_existing_generation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release = self.make_release(root)
            prefix = root / "installed"
            self.assertEqual(self.install(release, prefix).returncode, 0)
            (prefix / "versions" / VERSION / "bin" / "rw").write_bytes(b"changed")
            run = self.install(release, prefix)
            self.assertNotEqual(run.returncode, 0)
            self.assertEqual(
                (prefix / "versions" / VERSION / "bin" / "rw").read_bytes(), b"changed"
            )

    def test_refuses_archive_and_existing_generation_hardlinks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release = self.make_release(root)
            os.link(release / "bin" / "rw", release / "bin" / "rw-alias")
            prefix = root / "installed"
            run = self.install(release, prefix)
            self.assertNotEqual(run.returncode, 0)
            self.assertFalse(prefix.exists())

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release = self.make_release(root)
            prefix = root / "installed"
            self.assertEqual(self.install(release, prefix).returncode, 0)
            generation = prefix / "versions" / VERSION
            os.link(generation / "bin" / "rw", generation / "bin" / "rw-alias")
            run = self.install(release, prefix)
            self.assertNotEqual(run.returncode, 0)

    def test_install_lock_refuses_live_owner_and_recovers_dead_owner(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release = self.make_release(root)
            prefix = root / "installed"
            lock = prefix / ".install-lock"
            lock.mkdir(parents=True, mode=0o700)
            owner = lock / "pid"
            owner.write_text(f"{os.getpid()}\n", encoding="ascii")
            owner.chmod(0o600)
            run = self.install(release, prefix)
            self.assertNotEqual(run.returncode, 0)
            self.assertTrue(lock.is_dir())

            owner.write_text("2147483647\n", encoding="ascii")
            owner.chmod(0o600)
            run = self.install(release, prefix)
            self.assertEqual(run.returncode, 0, run.stderr.decode())
            self.assertFalse(lock.exists())
