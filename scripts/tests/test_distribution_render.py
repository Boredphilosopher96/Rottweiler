from __future__ import annotations

import hashlib
import io
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tarfile
import tempfile
import unittest

from scripts.release_contract import load_contract


REPO = Path(__file__).resolve().parents[2]
RENDERER = REPO / "scripts" / "render-distribution.py"
VERSION = "1.2.3"
PLATFORMS = ("darwin-arm64", "linux-x86_64")
RELEASE_CONTRACT = load_contract()


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
        binary_directory = tarfile.TarInfo(f"{release_root}/bin")
        binary_directory.type = tarfile.DIRTYPE
        binary_directory.mode = 0o755
        bundle.addfile(binary_directory)
        platform_contract = RELEASE_CONTRACT.platform(release_platform)
        for member in platform_contract.archive_members:
            content = installer if member.id == "installer" else member.id.encode("ascii")
            if member.id == "wasm_host_identity":
                content = json.dumps({"bytes": len(b"wasm_host"), "sha256": hashlib.sha256(b"wasm_host").hexdigest()}).encode()
            info = tarfile.TarInfo(f"{release_root}/{member.path}")
            info.mode = member.mode
            info.size = len(content)
            bundle.addfile(info, io.BytesIO(content))
    return archive


class DistributionRenderTests(unittest.TestCase):
    def render(
        self,
        root: Path,
        archives: list[tuple[str, Path]],
        *,
        formula: str = "rottweiler.rb",
        cask: str = "rottweiler.cask.rb",
        bootstrap: str = "rottweiler-install.sh",
    ) -> tuple[Path, Path, Path, subprocess.CompletedProcess[bytes]]:
        formula_path = root / formula
        cask_path = root / cask
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
        command.extend(
            [
                "--formula",
                str(formula_path),
                "--cask",
                str(cask_path),
                "--bootstrap",
                str(bootstrap_path),
            ]
        )
        run = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
        return formula_path, cask_path, bootstrap_path, run

    def test_render_is_deterministic_and_pins_every_exact_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archives = [(name, make_archive(root, name)) for name in PLATFORMS]
            first_formula, first_cask, first_bootstrap, first = self.render(root, archives)
            self.assertEqual(first.returncode, 0, first.stderr.decode())
            first_formula_bytes = first_formula.read_bytes()
            first_cask_bytes = first_cask.read_bytes()
            first_bootstrap_bytes = first_bootstrap.read_bytes()

            second_formula, second_cask, second_bootstrap, second = self.render(
                root,
                list(reversed(archives)),
                formula="second.rb",
                cask="second.cask.rb",
                bootstrap="second-install.sh",
            )
            self.assertEqual(second.returncode, 0, second.stderr.decode())
            self.assertEqual(first_formula_bytes, second_formula.read_bytes())
            self.assertEqual(first_cask_bytes, second_cask.read_bytes())
            self.assertEqual(first_bootstrap_bytes, second_bootstrap.read_bytes())
            self.assertEqual(first_bootstrap.stat().st_mode & 0o777, 0o755)

            formula_text = first_formula_bytes.decode()
            cask_text = first_cask_bytes.decode()
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
                if release_platform == "darwin-arm64":
                    self.assertIn(url, cask_text)
                    self.assertIn(digest, cask_text)

            self.assertIn('libexec.install Dir["bin/*"]', formula_text)
            self.assertIn(
                'bin.install_symlink libexec/"rw"',
                formula_text,
            )
            self.assertNotIn('bin.install "rottweiler-js-host"', formula_text)
            self.assertIn('refute_path_exists bin/"rottweiler-js-host"', formula_text)
            self.assertIn('libexec/"rottweiler-wasm-host"', formula_text)
            self.assertIn('libexec/"rottweiler-js-host"', formula_text)
            self.assertIn('license "Apache-2.0"', formula_text)
            self.assertIn("preserve_rpath", formula_text)
            self.assertIn("managed by Homebrew", formula_text)
            self.assertIn("brew upgrade", formula_text)
            self.assertIn('cask "rottweiler" do', cask_text)
            self.assertIn('depends_on arch: :arm64', cask_text)
            self.assertIn('binary "#{staged_path}/rottweiler-1.2.3-darwin-arm64/bin/rw", target: "rw"', cask_text)
            self.assertNotIn("ROTTWEILER_PACKAGE_MANAGER", cask_text)
            self.assertIn('system_command "/usr/bin/xattr"', cask_text)
            self.assertIn(
                'args: ["-dr", "com.apple.quarantine", "#{staged_path}/rottweiler-1.2.3-darwin-arm64"]',
                cask_text,
            )
            self.assertIn("This pre-v1 CLI is not Apple-notarized", cask_text)
            self.assertNotIn("--HEAD", cask_text)
            self.assertIn("--proto '=https'", bootstrap_text)
            self.assertIn("--proto-redir '=https'", bootstrap_text)
            self.assertIn("--tlsv1.2", bootstrap_text)
            self.assertIn("--max-redirs 5", bootstrap_text)
            self.assertIn('--max-filesize "$archive_length"', bootstrap_text)
            subprocess.run(["sh", "-n", str(first_bootstrap)], check=True)
            if subprocess.run(["sh", "-c", "command -v ruby"], check=False).returncode == 0:
                subprocess.run(["ruby", "-c", str(first_formula)], check=True)
                subprocess.run(["ruby", "-c", str(first_cask)], check=True)

    def test_bootstrap_verifies_then_invokes_bundled_installer_with_arguments(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archives = [(name, make_archive(root, name)) for name in PLATFORMS]
            _, _, bootstrap, rendered = self.render(root, archives)
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
            _, _, _, wrong_run = self.render(root, [("linux-x86_64", wrong)])
            self.assertNotEqual(wrong_run.returncode, 0)

            _, _, _, duplicate = self.render(
                root,
                [("linux-x86_64", good), ("linux-x86_64", good)],
            )
            self.assertNotEqual(duplicate.returncode, 0)

            link = root / f"rottweiler-{VERSION}-darwin-arm64.tar.gz"
            link.symlink_to(good)
            _, _, _, linked = self.render(root, [("darwin-arm64", link)])
            self.assertNotEqual(linked.returncode, 0)

            _, _, _, unsupported = self.render(root, [("windows-x86_64", good)])
            self.assertNotEqual(unsupported.returncode, 0)

            _, _, _, missing_os = self.render(root, [("linux-x86_64", good)])
            self.assertNotEqual(missing_os.returncode, 0)

    def test_renders_canonical_linux_aarch64_archive(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archives = [
                ("darwin-arm64", make_archive(root, "darwin-arm64")),
                ("linux-aarch64", make_archive(root, "linux-aarch64")),
            ]
            formula, _, bootstrap, rendered = self.render(root, archives)
            self.assertEqual(rendered.returncode, 0, rendered.stderr.decode())
            self.assertIn("rottweiler-1.2.3-linux-aarch64.tar.gz", formula.read_text())
            bootstrap_text = bootstrap.read_text()
            self.assertIn("Linux-aarch64)", bootstrap_text)
            self.assertIn("rottweiler-1.2.3-linux-aarch64", bootstrap_text)

    def test_head_formula_installs_verified_candidate_but_exposes_only_rw(self) -> None:
        formula = REPO / "packaging/homebrew/rottweiler-head.rb"
        text = formula.read_text(encoding="utf-8")
        self.assertIn('head "https://github.com/Boredphilosopher96/Rottweiler.git"', text)
        self.assertIn('depends_on "bun" => :build', text)
        self.assertIn('depends_on "rust" => :build', text)
        self.assertIn('depends_on "binutils" => :build', text)
        self.assertIn('ROTTWEILER_STRIP_BIN: formula_opt_bin("binutils")/"strip"', text)
        self.assertIn("preserve_rpath", text)
        self.assertIn('"scripts/build-native-candidate.py"', text)
        self.assertIn('"scripts/native_candidate.py", "path", candidate, "engine"', text)
        self.assertIn('libexec.install Dir[(engine.dirname/"*").to_s]', text)
        self.assertNotIn('"scripts/cargo-release.sh", "build"', text)
        self.assertNotIn('"bun", "run"', text)
        self.assertIn(
            'bin.install_symlink libexec/"rw"',
            text,
        )
        self.assertNotIn('bin.install "rottweiler-js-host"', text)
        self.assertIn('refute_path_exists bin/"rottweiler-js-host"', text)
        self.assertIn("managed by Homebrew", text)
        self.assertIn("brew upgrade", text)
        if subprocess.run(["sh", "-c", "command -v ruby"], check=False).returncode == 0:
            subprocess.run(["ruby", "-c", str(formula)], check=True)

        tui_build = (REPO / "packages/js-host/build.ts").read_text(encoding="utf-8")
        self.assertIn('process.env.ROTTWEILER_STRIP_BIN ?? "/usr/bin/strip"', tui_build)
        self.assertIn("isAbsolute(stripExecutable)", tui_build)
        self.assertIn('"/usr/bin/codesign"', tui_build)
        self.assertIn('"--timestamp=none"', tui_build)


if __name__ == "__main__":
    unittest.main()
