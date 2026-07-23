from __future__ import annotations

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[2]
WRAPPER = REPO / "crates/rw-cli/tests/m8_release_gate_linux.sh"


class M8LinuxWrapperTests(unittest.TestCase):
    def test_forwards_metrics_and_uses_ephemeral_tmpfs_builds(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            log = root / "docker.log"
            docker = root / "docker"
            docker.write_text(
                "#!/bin/sh\n"
                "printf '%s\\n' \"$*\" >> \"$DOCKER_LOG\"\n"
                "exit 0\n",
                encoding="utf-8",
            )
            docker.chmod(0o700)
            env = {
                **os.environ,
                "PATH": f"{root}:{os.environ['PATH']}",
                "DOCKER_LOG": str(log),
                "ROTTWEILER_PERF_OUTPUT": "m8-test.json",
                "ROTTWEILER_UPDATE_ROOT_VERSION": "7",
            }
            env.pop("ROTTWEILER_UPDATE_ROOT_THRESHOLD", None)

            subprocess.run([str(WRAPPER)], cwd=REPO, env=env, check=True)

            log_text = log.read_text(encoding="utf-8")
            calls = log_text.splitlines()
            run = next(call for call in calls if call.startswith("run "))
            self.assertIn("--privileged", run)
            self.assertIn(f"type=bind,source={REPO},target={REPO}", run)
            self.assertIn("--tmpfs /m8-work:rw,exec,size=3g", run)
            self.assertIn("--env ROTTWEILER_PERF_OUTPUT", run)
            self.assertIn("--env ROTTWEILER_UPDATE_ROOT_VERSION", run)
            self.assertNotIn("ROTTWEILER_UPDATE_ROOT_THRESHOLD", run)
            self.assertIn(
                "docker.io/library/rust:1.94.1-bookworm@sha256:"
                "6ae102bdbf528294bc79ad6e1fae682f6f7c2a6e6621506ba959f9685b308a55",
                run,
            )
            self.assertIn("CARGO_PROFILE_DEV_DEBUG=0", log_text)
            self.assertIn("link-arg=-fuse-ld=gold", log_text)
            self.assertIn('rm -rf "$CARGO_TARGET_DIR"', log_text)
            self.assertIn("m8_release_gate.py", log_text)
            self.assertFalse(
                any("rottweiler-m8-target-" in call for call in calls),
                "build output must never use a leakable named volume",
            )

    def test_rejects_a_metrics_path_outside_the_workspace_bind(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            env = {
                **os.environ,
                "ROTTWEILER_PERF_OUTPUT": str(Path(temporary) / "m8.json"),
            }
            run = subprocess.run(
                [str(WRAPPER)],
                cwd=REPO,
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                check=False,
            )

            self.assertEqual(run.returncode, 2)
            self.assertIn("must remain inside", run.stderr)

    def test_rejects_metrics_in_functional_only_mode(self) -> None:
        env = {
            **os.environ,
            "ROTTWEILER_M8_FUNCTIONAL_ONLY": "1",
            "ROTTWEILER_PERF_OUTPUT": "m8-test.json",
        }
        run = subprocess.run(
            [str(WRAPPER)],
            cwd=REPO,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )

        self.assertEqual(run.returncode, 2)
        self.assertIn("requires the complete", run.stderr)

    def test_linux_workflow_call_sites_use_the_privileged_wrapper(self) -> None:
        ci = (REPO / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        protected_performance = (
            REPO / ".github/workflows/performance.yml"
        ).read_text(encoding="utf-8")
        nightly = (REPO / ".github/workflows/nightly.yml").read_text(encoding="utf-8")
        release = (REPO / ".github/workflows/release.yml").read_text(encoding="utf-8")

        main_test = ci.split("  test:", 1)[1].split("  security-tests:", 1)[0]
        security = ci.split("  security-tests:", 1)[1].split(
            "  performance-smoke:", 1
        )[0]
        performance = protected_performance.split("  performance-linux:", 1)[1].split(
            "  performance-macos:", 1
        )[0]
        release_linux = release.split("  build-linux:", 1)[1].split(
            "  build-macos:", 1
        )[0]
        nightly_release = nightly.split("  linux-release-budget:", 1)[1].split(
            "  macos-release-budget:", 1
        )[0]
        self.assertNotIn("m8_release_gate_linux.sh", main_test)
        self.assertIn("m8_release_gate_linux.sh", security)
        self.assertIn("ROTTWEILER_M8_FUNCTIONAL_ONLY: 1", security)
        self.assertIn("persist-credentials: false", security)
        self.assertIn("persist-credentials: false", performance)
        self.assertIn("persist-credentials: false", nightly_release)
        self.assertIn("needs: [test, security-tests]", ci)
        self.assertIn("m8_release_gate_linux.sh", nightly)
        self.assertIn("m8_release_gate_linux.sh", release_linux)


if __name__ == "__main__":
    unittest.main()
