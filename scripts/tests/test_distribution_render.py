from __future__ import annotations

import hashlib
import io
import os
from pathlib import Path
import platform
import subprocess
import sys
import tarfile
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[2]
RENDERER = REPO / "scripts" / "render-distribution.py"
VERSION = "1.2.3"
PLATFORMS = ("darwin-arm64", "linux-x86_64")


def make_archive(root: Path, release_platform: str) -> Path:
    archive = root / f"rottweiler-{VERSION}-{release_platform}.tar.gz"
    release_root = f"rottweiler-{VERSION}-{release_platform}"
    installer = (
        "#!/bin/sh\n"
        "set -eu\n"
        "printf '%s\\n' \"$*\" > \"$BOOTSTRAP_LOG\"\n"
    ).encode()
    with tarfile.open(archive, "w:gz") as bundle:
        directory = tarfile.TarInfo(release_root)
        directory.type = tarfile.DIRTYPE
        directory.mode = 0o755
        bundle.addfile(directory)
        info = tarfile.TarInfo(f"{release_root}/install.sh")
        info.mode = 0o755
        info.size = len(installer)
        bundle.addfile(info, io.BytesIO(installer))
    return archive


class DistributionRenderTests(unittest.TestCase):
    def render(
        self,
        root: Path,
        archives: list[tuple[str, Path]],
        *,
        formula: str = "rottweiler.rb",
        bootstrap: str = "rottweiler-install.sh",
    ) -> tuple[Path, Path, subprocess.CompletedProcess[bytes]]:
        formula_path = root / formula
        bootstrap_path = root / bootstrap
        command = [
            sys.executable,
            str(RENDERER),
            "--version",
            VERSION,
            "--repository",
            "Boredphilosopher96/Rottweiler",
        ]
        for release_platform, archive in archives:
            command.extend(["--archive", f"{release_platform}={archive}"])
        command.extend(["--formula", str(formula_path), "--bootstrap", str(bootstrap_path)])
        run = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
        return formula_path, bootstrap_path, run

    def test_render_is_deterministic_and_pins_every_exact_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archives = [(name, make_archive(root, name)) for name in PLATFORMS]
            first_formula, first_bootstrap, first = self.render(root, archives)
            self.assertEqual(first.returncode, 0, first.stderr.decode())
            first_formula_bytes = first_formula.read_bytes()
            first_bootstrap_bytes = first_bootstrap.read_bytes()

            second_formula, second_bootstrap, second = self.render(
                root,
                list(reversed(archives)),
                formula="second.rb",
                bootstrap="second-install.sh",
            )
            self.assertEqual(second.returncode, 0, second.stderr.decode())
            self.assertEqual(first_formula_bytes, second_formula.read_bytes())
            self.assertEqual(first_bootstrap_bytes, second_bootstrap.read_bytes())
            self.assertEqual(first_bootstrap.stat().st_mode & 0o777, 0o755)

            formula_text = first_formula_bytes.decode()
            bootstrap_text = first_bootstrap_bytes.decode()
            for release_platform, archive in archives:
                url = (
                    "https://github.com/Boredphilosopher96/Rottweiler/releases/download/"
                    f"v{VERSION}/{archive.name}"
                )
                digest = hashlib.sha256(archive.read_bytes()).hexdigest()
                self.assertIn(url, formula_text)
                self.assertIn(digest, formula_text)
                self.assertIn(url, bootstrap_text)
                self.assertIn(f"archive_length='{archive.stat().st_size}'", bootstrap_text)
                self.assertIn(f"archive_sha256='{digest}'", bootstrap_text)

            self.assertIn('libexec.install Dir["bin/*"]', formula_text)
            self.assertIn(
                '(bin/"rw").write_env_script libexec/"rw", ROTTWEILER_PACKAGE_MANAGER: "homebrew"',
                formula_text,
            )
            self.assertNotIn('bin.install "rottweiler-tui"', formula_text)
            self.assertIn('refute_path_exists bin/"rottweiler-tui"', formula_text)
            self.assertIn('license "Apache-2.0"', formula_text)
            self.assertIn("preserve_rpath", formula_text)
            self.assertIn("managed by Homebrew", formula_text)
            self.assertIn("brew upgrade", formula_text)
            self.assertIn("--proto '=https'", bootstrap_text)
            self.assertIn("--proto-redir '=https'", bootstrap_text)
            self.assertIn("--tlsv1.2", bootstrap_text)
            self.assertIn("--max-redirs 5", bootstrap_text)
            self.assertIn('--max-filesize "$archive_length"', bootstrap_text)
            subprocess.run(["sh", "-n", str(first_bootstrap)], check=True)
            if subprocess.run(["sh", "-c", "command -v ruby"], check=False).returncode == 0:
                subprocess.run(["ruby", "-c", str(first_formula)], check=True)

    def test_bootstrap_verifies_then_invokes_bundled_installer_with_arguments(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archives = [(name, make_archive(root, name)) for name in PLATFORMS]
            _, bootstrap, rendered = self.render(root, archives)
            self.assertEqual(rendered.returncode, 0, rendered.stderr.decode())
            current = {
                ("Darwin", "arm64"): archives[0][1],
                ("Linux", "x86_64"): archives[1][1],
            }.get((platform.system(), platform.machine()))
            if current is None:
                self.skipTest("bootstrap fixture does not publish this host platform")

            fake_bin = root / "fake-bin"
            fake_bin.mkdir()
            curl = fake_bin / "curl"
            curl.write_text(
                "#!/bin/sh\n"
                "set -eu\n"
                "output=\n"
                "want_output=0\n"
                "for argument do\n"
                "  if [ \"$want_output\" = 1 ]; then output=$argument; want_output=0; fi\n"
                "  [ \"$argument\" != --output ] || want_output=1\n"
                "done\n"
                "[ -n \"$output\" ]\n"
                "cp \"$FAKE_ARCHIVE\" \"$output\"\n",
                encoding="utf-8",
            )
            curl.chmod(0o755)
            log = root / "installed.log"
            environment = os.environ.copy()
            environment.update(
                {
                    "PATH": f"{fake_bin}:{environment.get('PATH', '/usr/bin:/bin')}",
                    "FAKE_ARCHIVE": str(current),
                    "BOOTSTRAP_LOG": str(log),
                }
            )
            run = subprocess.run(
                [str(bootstrap), "--prefix", str(root / "prefix")],
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(run.returncode, 0, run.stderr.decode())
            self.assertEqual(log.read_text(encoding="utf-8"), f"--prefix {root / 'prefix'}\n")

            corrupted = root / "corrupted.tar.gz"
            content = bytearray(current.read_bytes())
            content[len(content) // 2] ^= 1
            corrupted.write_bytes(content)
            log.unlink()
            environment["FAKE_ARCHIVE"] = str(corrupted)
            failed = subprocess.run(
                [str(bootstrap), "--prefix", str(root / "prefix")],
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertNotEqual(failed.returncode, 0)
            self.assertIn(b"SHA-256 mismatch", failed.stderr)
            self.assertFalse(log.exists())

    def test_rejects_wrong_names_duplicates_symlinks_and_unsupported_platforms(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            good = make_archive(root, "linux-x86_64")
            wrong = root / "wrong.tar.gz"
            wrong.write_bytes(b"archive")
            _, _, wrong_run = self.render(root, [("linux-x86_64", wrong)])
            self.assertNotEqual(wrong_run.returncode, 0)

            _, _, duplicate = self.render(
                root,
                [("linux-x86_64", good), ("linux-x86_64", good)],
            )
            self.assertNotEqual(duplicate.returncode, 0)

            link = root / f"rottweiler-{VERSION}-darwin-arm64.tar.gz"
            link.symlink_to(good)
            _, _, linked = self.render(root, [("darwin-arm64", link)])
            self.assertNotEqual(linked.returncode, 0)

            _, _, unsupported = self.render(root, [("windows-x86_64", good)])
            self.assertNotEqual(unsupported.returncode, 0)

            _, _, missing_os = self.render(root, [("linux-x86_64", good)])
            self.assertNotEqual(missing_os.returncode, 0)

    def test_head_formula_builds_both_components_but_exposes_only_rw(self) -> None:
        formula = REPO / "packaging/homebrew/rottweiler-head.rb"
        text = formula.read_text(encoding="utf-8")
        self.assertIn('head "https://github.com/Boredphilosopher96/Rottweiler.git"', text)
        self.assertIn('depends_on "bun" => :build', text)
        self.assertIn('depends_on "rust" => :build', text)
        self.assertIn('depends_on "binutils" => :build', text)
        self.assertIn('ROTTWEILER_STRIP_BIN: formula_opt_bin("binutils")/"strip"', text)
        self.assertIn("preserve_rpath", text)
        self.assertIn('"scripts/cargo-release.sh", "build", "--locked", "--release"', text)
        self.assertIn('libexec.install "packages/tui/dist/rottweiler-tui"', text)
        self.assertIn(
            '(bin/"rw").write_env_script libexec/"rw", ROTTWEILER_PACKAGE_MANAGER: "homebrew"',
            text,
        )
        self.assertNotIn('bin.install "rottweiler-tui"', text)
        self.assertIn('refute_path_exists bin/"rottweiler-tui"', text)
        self.assertIn("managed by Homebrew", text)
        self.assertIn("brew upgrade", text)
        if subprocess.run(["sh", "-c", "command -v ruby"], check=False).returncode == 0:
            subprocess.run(["ruby", "-c", str(formula)], check=True)

        tui_build = (REPO / "packages/tui/build.ts").read_text(encoding="utf-8")
        self.assertIn('process.env.ROTTWEILER_STRIP_BIN ?? "/usr/bin/strip"', tui_build)
        self.assertIn("isAbsolute(stripExecutable)", tui_build)
        self.assertIn('"/usr/bin/codesign"', tui_build)
        self.assertIn('"--timestamp=none"', tui_build)


if __name__ == "__main__":
    unittest.main()
