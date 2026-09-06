from __future__ import annotations

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[2]
WRAPPER = REPO / "crates/rw-cli/tests/m8_release_gate_linux.sh"


class M8LinuxWrapperTests(unittest.TestCase):
    def test_native_measurement_consumes_explicit_artifacts_without_compilation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            engine, fixture = root / "engine", root / "fixture"
            engine.write_bytes(b"prepared engine")
            fixture.write_bytes(b"prepared fixture")
            log = root / "arguments"
            driver = root / "python3"
            driver.write_text(
                '#!/bin/sh\n'
                'printf "%s\\n" "$@" > "$M8_LOG"\n'
                'cp "$3" "$M8_ENGINE_CAPTURE"\n'
                'cp "$5" "$M8_FIXTURE_CAPTURE"\n'
            )
            driver.chmod(0o700)
            for name in ("cargo", "rustc"):
                compiler = root / name
                compiler.write_text('#!/bin/sh\nexit 97\n')
                compiler.chmod(0o700)
            env = {**os.environ, "PATH": f"{root}:{os.environ['PATH']}",
                   "M8_LOG": str(log), "M8_ENGINE_CAPTURE": str(root / "copied-engine"),
                   "M8_FIXTURE_CAPTURE": str(root / "copied-fixture"),
                   "ROTTWEILER_M8_PERF_SAMPLES": "100", "ROTTWEILER_M8_FUNCTIONAL_ONLY": "0"}
            env.pop("ROTTWEILER_PERF_OUTPUT", None)
            wrapper = REPO / "crates/rw-cli/tests/m8_release_gate.sh"
            subprocess.run([str(wrapper), str(engine), str(fixture)], env=env, check=True)
            arguments = log.read_text().splitlines()
            self.assertEqual((root / "copied-engine").read_bytes(), engine.read_bytes())
            self.assertEqual((root / "copied-fixture").read_bytes(), fixture.read_bytes())
            self.assertEqual(arguments[-2:], ["--samples", "100"])
            self.assertFalse(Path(arguments[2]).parent.exists(), "private copies must be cleaned")
            missing = subprocess.run([str(wrapper), str(engine)], env=env, capture_output=True)
            self.assertEqual(missing.returncode, 2)
            self.assertIn(b"MCP_FIXTURE_EXECUTABLE", missing.stderr)

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
                "docker.io/library/rust:1.97.1-bookworm@sha256:"
                "77fac8b98f9f46062bb680b6d25d5bcaabfc400143952ebc572e924bcbedc3fa",
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
        preflight = (REPO / ".github/workflows/release-preflight.yml").read_text(
            encoding="utf-8"
        )
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
        aggregate = ci.split("  required:", 1)[1]
        self.assertIn("security-tests", aggregate)
        self.assertIn("performance-smoke", aggregate)
        self.assertIn("if: always()", aggregate)
        self.assertIn("m8_release_gate_linux.sh", nightly)
        self.assertIn("uses: ./.github/workflows/performance.yml", preflight)
        self.assertNotIn("m8_release_gate_linux.sh", release_linux)


if __name__ == "__main__":
    unittest.main()
