import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
WRAPPER = ROOT / "scripts" / "cargo-release.sh"


class CargoReleaseTests(unittest.TestCase):
    def run_fixture(self, host: str) -> tuple[list[str], dict[str, str], Path]:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            tools = root / "bin"
            tools.mkdir()
            log = root / "cargo.json"
            target = root / "target output"
            (tools / "rustc").write_text(
                f"#!/bin/sh\nprintf 'rustc 1.94.1\\nhost: {host}\\n'\n",
                encoding="utf-8",
            )
            (tools / "cargo").write_text(
                "#!/bin/sh\n"
                "python3 - \"$@\" <<'PY'\n"
                "import json, os, sys\n"
                "json.dump({'argv': sys.argv[1:], 'env': dict(os.environ)}, open(os.environ['RW_CARGO_LOG'], 'w'))\n"
                "PY\n",
                encoding="utf-8",
            )
            (tools / "rustc").chmod(0o700)
            (tools / "cargo").chmod(0o700)
            env = {
                **os.environ,
                "PATH": f"{tools}:{os.environ['PATH']}",
                "RW_CARGO_LOG": str(log),
                "CARGO_TARGET_DIR": str(target),
            }
            subprocess.run(
                [str(WRAPPER), "build", "--locked", "--release", "-p", "rw-cli"],
                cwd=ROOT,
                env=env,
                check=True,
            )
            artifact = subprocess.run(
                [str(WRAPPER), "artifact-dir"],
                cwd=ROOT,
                env=env,
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
            payload = json.loads(log.read_text(encoding="utf-8"))
            return payload["argv"], payload["env"], Path(artifact)

    def test_native_profiles_force_host_target_and_exact_artifact_directory(self) -> None:
        for host, optimization in [
            ("aarch64-apple-darwin", "z"),
            ("x86_64-unknown-linux-gnu", "s"),
        ]:
            with self.subTest(host=host):
                argv, env, artifact = self.run_fixture(host)
                self.assertEqual(
                    argv,
                    [
                        "build",
                        "--target",
                        host,
                        "--target-dir",
                        str(artifact.parents[1]),
                        "--locked",
                        "--release",
                        "-p",
                        "rw-cli",
                    ],
                )
                self.assertEqual(env["CARGO_PROFILE_RELEASE_OPT_LEVEL"], optimization)
                self.assertEqual(artifact.name, "release")
                self.assertEqual(artifact.parent.name, host)
                self.assertEqual(Path(env["CARGO_TARGET_DIR"]), artifact.parents[1])

    def test_cross_target_overrides_fail_before_cargo(self) -> None:
        env = {**os.environ, "CARGO_BUILD_TARGET": "x86_64-unknown-linux-gnu"}
        run = subprocess.run(
            [str(WRAPPER), "artifact-dir"],
            cwd=ROOT,
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(run.returncode, 2)
        self.assertIn("CARGO_BUILD_TARGET is unsupported", run.stderr)

        run = subprocess.run(
            [str(WRAPPER), "build", "--target=x86_64-unknown-linux-gnu"],
            cwd=ROOT,
            capture_output=True,
            text=True,
        )
        self.assertEqual(run.returncode, 2)
        self.assertIn("--target is unsupported", run.stderr)

        run = subprocess.run(
            [str(WRAPPER), "build", "--release", "--target-dir=/tmp/elsewhere"],
            cwd=ROOT,
            capture_output=True,
            text=True,
        )
        self.assertEqual(run.returncode, 2)
        self.assertIn("--target-dir is owned", run.stderr)

        run = subprocess.run(
            [str(WRAPPER), "build", "--locked"],
            cwd=ROOT,
            capture_output=True,
            text=True,
        )
        self.assertEqual(run.returncode, 2)
        self.assertIn("require --release", run.stderr)


if __name__ == "__main__":
    unittest.main()
