from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("native_size_diagnostic", SCRIPTS / "diagnose-native-size.py")
diagnostic = importlib.util.module_from_spec(spec)
spec.loader.exec_module(diagnostic)


class NativeSizeDiagnosticsTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.repo = Path(temporary.name).resolve()
        self.target = self.repo / "target"
        self.engine = self.target / "x86_64-unknown-linux-gnu/release/rw"
        self.engine.parent.mkdir(parents=True)
        self.original = b"\x7fELFexact failing artifact"
        self.engine.write_bytes(self.original)
        self.output = self.repo / "diagnostics"
        self.gate = self.repo / "build.json"
        self.gate.write_text(json.dumps({"status": "failed", "exit_code": 1, "source_sha": "abc",
                                         "log_tail": f"ValueError: release engine is {len(self.original)} bytes; product budget is <10"}))
        self.identity = {"platform": "linux-x86_64", "target": "x86_64-unknown-linux-gnu",
                         "source": {"commit": "abc", "tree_sha256": "tree"},
                         "profile": {"name": "release", "opt_level": "s", "debug": 0}}

    def run_diagnostic(self, relink):
        with patch.object(diagnostic.native_candidate, "build_identity", return_value=self.identity), \
             patch.object(diagnostic.native_candidate, "source_identity", return_value=self.identity["source"]), \
             patch.object(diagnostic.ci_evidence, "observe", side_effect=relink), \
             patch.dict(os.environ, {"SOURCE_DATE_EPOCH": "1"}):
            return diagnostic.diagnose(self.repo, self.target, self.output, self.gate)

    def test_preserves_failing_elf_before_same_graph_final_target_relink(self):
        original_gate = self.gate.read_bytes()

        def relink(command, _name, _result):
            self.assertEqual((self.output / "failed-engine.elf").read_bytes(), self.original)
            self.assertEqual(json.loads((self.output / "failure.json").read_text())["identity"], self.identity)
            self.assertEqual(command[:2], ["cargo", "rustc"])
            self.assertEqual(command[command.index("--") + 1:],
                             ["-C", "link-arg=-Wl,-Map=" + str(self.output / "engine.map"),
                              *diagnostic.native_profile.final_rustflags(self.identity["target"])])
            self.assertNotIn("--all-features", command)
            self.assertEqual(os.environ["CARGO_PROFILE_RELEASE_OPT_LEVEL"], "s")
            self.assertEqual(os.environ["CARGO_PROFILE_RELEASE_DEBUG"], "0")
            self.assertEqual(os.environ["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")[-4:],
                             ["-C", "force-unwind-tables=no", "-C", "link-arg=-Wl,-z,pack-relative-relocs"])
            self.engine.write_bytes(b"\x7fELFdiagnostic artifact")
            (self.output / "engine.map").write_text("actual linker map")
            return 0

        self.assertEqual(self.run_diagnostic(relink), 0)
        result = json.loads((self.output / "failure.json").read_text())
        self.assertNotEqual(result["original"]["sha256"], result["diagnostic_engine"]["sha256"])
        self.assertEqual(self.gate.read_bytes(), original_gate)
        self.assertEqual((self.output / "failed-engine.elf").read_bytes(), self.original)

    def test_diagnostic_failure_cannot_replace_original_gate_or_artifact(self):
        original_gate = self.gate.read_bytes()
        self.assertEqual(self.run_diagnostic(lambda *_: 23), 23)
        self.assertEqual(self.gate.read_bytes(), original_gate)
        self.assertEqual((self.output / "failed-engine.elf").read_bytes(), self.original)
        self.assertEqual(json.loads((self.output / "failure.json").read_text())["diagnostic_exit_code"], 23)

    def test_successful_gate_cannot_trigger_diagnostic_build(self):
        self.gate.write_text(json.dumps({"status": "passed", "exit_code": 0, "source_sha": "abc"}))
        with self.assertRaisesRegex(ValueError, "failed build gate"):
            self.run_diagnostic(lambda *_: self.fail("must not relink a successful candidate"))
        self.assertFalse(self.output.exists())

    def test_other_build_failure_cannot_relabel_a_cached_engine_as_size_evidence(self):
        self.gate.write_text(json.dumps({"status": "failed", "exit_code": 1, "source_sha": "abc",
                                         "log_tail": "rustc failed before linking"}))
        with self.assertRaisesRegex(ValueError, "engine size-gate failure"):
            self.run_diagnostic(lambda *_: self.fail("must not relink an unrelated failure"))
        self.assertFalse(self.output.exists())

    def test_changed_artifact_cannot_be_substituted_for_reported_size_failure(self):
        self.engine.write_bytes(self.original + b"changed")
        with self.assertRaisesRegex(ValueError, "artifact size differs"):
            self.run_diagnostic(lambda *_: self.fail("must not relink changed artifact"))
        self.assertFalse(self.output.exists())

    def test_ci_failure_step_is_diagnostic_only_and_evidence_upload_is_unconditional(self):
        workflow = (SCRIPTS.parent / ".github/workflows/ci.yml").read_text()
        linux = workflow.split("  linux-candidate-build:", 1)[1].split("  macos-candidate-build:", 1)[0]
        self.assertIn("if: failure() && steps.candidate.outcome == 'failure'", linux)
        self.assertIn("continue-on-error: true", linux)
        self.assertIn("--failed-gate ci-results/build.json", linux)
        self.assertIn("--output ci-results/native-size", linux)
        self.assertIn("Retain build and size evidence\n        if: always()", linux)


if __name__ == "__main__":
    unittest.main()
