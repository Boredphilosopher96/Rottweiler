"""Keep the Mach FFI exception explicit and ordinary workspace crates safe."""
from pathlib import Path
import tomllib
import unittest
import yaml

ROOT = Path(__file__).resolve().parents[2]
BOUNDARY = "crates/rw-macos-bootstrap"


class RustPlatformBoundaryTests(unittest.TestCase):
    def test_workspace_owns_every_crate_and_one_explicit_ffi_exception(self):
        workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())
        self.assertEqual(workspace["workspace"]["lints"]["rust"]["unsafe_code"], "forbid")
        members = set(workspace["workspace"]["members"])
        self.assertIn(BOUNDARY, members)
        actual = {str(path.parent.relative_to(ROOT)) for path in (ROOT / "crates").glob("*/Cargo.toml")}
        self.assertEqual(members, actual)
        for member in sorted(members):
            manifest = tomllib.loads((ROOT / member / "Cargo.toml").read_text())
            if member != BOUNDARY:
                self.assertTrue(manifest["lints"]["workspace"], member)
            else:
                self.assertEqual(manifest["lints"]["rust"]["unsafe_code"], "deny")
                self.assertEqual(manifest["lints"]["rust"]["unsafe_op_in_unsafe_fn"], "deny")
                self.assertEqual(manifest["lints"]["clippy"]["undocumented_unsafe_blocks"], "deny")
        source = (ROOT / BOUNDARY / "src/lib.rs").read_text()
        self.assertIn('#![deny(unsafe_code)]', source)
        self.assertIn('#[allow(unsafe_code)]\nmod authority;', source)
        unsafe_modules = [path for path in (ROOT / "crates").glob("*/src/**/*.rs")
                          if '#[allow(unsafe_code)]' in path.read_text()]
        self.assertEqual(unsafe_modules, [ROOT / BOUNDARY / "src/lib.rs"])

    def test_workspace_gates_compile_the_platform_crate_on_both_native_os_jobs(self):
        workflow = yaml.safe_load((ROOT / ".github/workflows/ci.yml").read_text())
        job = workflow["jobs"]["test"]
        systems = job["strategy"]["matrix"]["os"]
        self.assertTrue(any(value.startswith("macos-") for value in systems))
        self.assertTrue(any(value.startswith("ubuntu-") for value in systems))
        commands = "\n".join(step.get("run", "") for step in job["steps"])
        self.assertRegex(commands, r"cargo test[^\n]*--workspace")
        self.assertRegex(commands, r"cargo clippy[^\n]*--workspace")

    def test_native_plugin_tests_prepare_the_worker_before_execution(self):
        for workflow_name, job_name, consumer in [
            ("ci.yml", "test", "cargo test"),
            ("quality.yml", "rust-coverage", "cargo llvm-cov"),
            ("quality.yml", "security-mutation", "cargo mutants"),
            ("release.yml", "release-gate", "cargo test"),
        ]:
            workflow = yaml.safe_load((ROOT / ".github/workflows" / workflow_name).read_text())
            commands = [step.get("run", "") for step in workflow["jobs"][job_name]["steps"]]
            producer = next(index for index, command in enumerate(commands)
                            if "build-test-helper.py --github-env" in command)
            consumers = [index for index, command in enumerate(commands) if consumer in command]
            self.assertTrue(consumers, workflow_name)
            self.assertTrue(all(producer < index for index in consumers), workflow_name)
            self.assertIn('"$GITHUB_ENV"', commands[producer])
            prerequisite = next(index for index, command in enumerate(commands)
                                if "unshare --user" in command)
            self.assertLess(prerequisite, producer, workflow_name)
            self.assertIn("iproute2 util-linux", commands[prerequisite])
            self.assertIn("ROTTWEILER_REQUIRE_LINUX_SANDBOX=1", commands[prerequisite])
            self.assertNotIn("|| true", commands[prerequisite])
