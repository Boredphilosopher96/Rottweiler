"""Cargo prerequisite evidence is bounded and physically settled before publication."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
SPEC = importlib.util.spec_from_file_location(
    "test_helper_build", Path(__file__).resolve().parents[1] / "build-test-helper.py")
HELPER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HELPER)
PROFILE = {"opt_level": "0", "debuginfo": "line-tables-only", "debug_assertions": True,
           "overflow_checks": True, "test": False}
FINISHED = {"reason": "build-finished", "success": True}


def fixture(root):
    (root / "Cargo.toml").write_text('[profile.dev]\ndebug = "line-tables-only"\n')
    manifest = root / "crates/rw-sandbox/Cargo.toml"
    manifest.parent.mkdir(parents=True)
    manifest.write_text('[package]\nname = "rw-sandbox"\n')
    target = root / "target/debug"
    target.mkdir(parents=True)
    messages = []
    for name, relative in [(HELPER.BINARY, "src/bin/rw-sandbox-helper.rs"),
                           (HELPER.FIXTURE, "tests/fixtures/ownership.rs")]:
        source = manifest.parent / relative
        source.parent.mkdir(parents=True, exist_ok=True)
        source.write_text("fn main() {}")
        executable = target / name
        executable.write_bytes(b"executable fixture")
        executable.chmod(0o700)
        messages.append({"reason": "compiler-artifact", "manifest_path": str(manifest),
            "target": {"name": name, "kind": ["bin"], "crate_types": ["bin"], "src_path": str(source)},
            "profile": dict(PROFILE), "executable": str(executable)})
    return target.resolve(), messages


def encoded(messages):
    return b"\n".join(json.dumps(message).encode() for message in messages) + b"\n"


class HelperBuildTests(unittest.TestCase):
    def test_exact_two_source_bound_artifacts_and_one_completion(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target, messages = fixture(root)
            with mock.patch.object(HELPER, "ROOT", root):
                self.assertEqual(HELPER.cargo_artifacts(0, encoded(messages + [FINISHED]), target, PROFILE),
                                 {name: target / name for name in HELPER.BINARIES})
                for records, code in [(messages, 0), ([messages[0], FINISHED], 0),
                        (messages + [messages[0], FINISHED], 0), (messages + [FINISHED, FINISHED], 0),
                        (messages + [{"reason": "build-finished", "success": False}], 0),
                        (messages + [{"reason": "build-finished", "success": 1}], 0),
                        (messages + [FINISHED], 1), ([[]], 0)]:
                    with self.subTest(records=records, code=code), self.assertRaises(ValueError):
                        HELPER.cargo_artifacts(code, encoded(records), target, PROFILE)
                for body in [b"malformed\n", b'{"reason":"a","reason":"build-finished"}\n',
                             b" " * (1024 * 1024 + 1)]:
                    with self.subTest(body=body[:40]), self.assertRaises(ValueError):
                        HELPER.cargo_artifacts(0, body, target, PROFILE)

    def test_rejects_wrong_profile_source_target_and_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target, messages = fixture(root)
            changes = [lambda event: event["profile"].update(test=True),
                       lambda event: event["profile"].update(opt_level="3"),
                       lambda event: event["profile"].update(debug_assertions=False),
                       lambda event: event["target"].update(kind=["test"]),
                       lambda event: event["target"].update(src_path=messages[1]["target"]["src_path"]),
                       lambda event: event.update(manifest_path=str(root / "Cargo.toml")),
                       lambda event: event.update(executable=str(target / HELPER.FIXTURE))]
            with mock.patch.object(HELPER, "ROOT", root):
                for change in changes:
                    records = json.loads(json.dumps(messages))
                    change(records[0])
                    with self.subTest(change=change), self.assertRaises(ValueError):
                        HELPER.cargo_artifacts(0, encoded(records + [FINISHED]), target, PROFILE)
                other = root / "other"
                other.mkdir()
                with self.assertRaises(ValueError):
                    HELPER.cargo_artifacts(0, encoded(messages + [FINISHED]), other, PROFILE)
                executable = target / HELPER.BINARY
                executable.unlink()
                executable.symlink_to(target / HELPER.FIXTURE)
                with self.assertRaises(ValueError):
                    HELPER.cargo_artifacts(0, encoded(messages + [FINISHED]), target, PROFILE)

    def test_build_uses_shared_owner_and_rejects_source_change_before_publication(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target, messages = fixture(root)
            result = subprocess.CompletedProcess([], 0, encoded(messages + [FINISHED]), b"diagnostics")
            evidence = root / "evidence"
            evidence.mkdir()
            with mock.patch.object(HELPER, "ROOT", root), mock.patch.dict(os.environ, {}, clear=True), \
                    mock.patch.object(HELPER.native_candidate, "source_identity", return_value={"source": "one"}) as identity, \
                    mock.patch.object(HELPER.native_candidate, "configuration_fingerprints", return_value={}), \
                    mock.patch.object(HELPER.tempfile, "mkdtemp", return_value=str(evidence)), \
                    mock.patch.object(HELPER, "run_sample", return_value=result) as run:
                self.assertEqual(HELPER.build(), {name: target / name for name in HELPER.BINARIES})
                self.assertEqual(run.call_args.kwargs["timeout"], 7200)
                self.assertEqual(run.call_args.kwargs["output_limit"], 64 * 1024 * 1024)
                self.assertEqual((evidence / "stdout.jsonl").read_bytes(), result.stdout)
                self.assertEqual((evidence / "stderr.log").read_bytes(), result.stderr)
                self.assertEqual(run.call_args.args[0][-1], "--message-format=json-render-diagnostics")
                identity.side_effect = [{"source": "one"}, {"source": "changed"}]
                with self.assertRaisesRegex(ValueError, "source or Cargo configuration changed"):
                    HELPER.build()
                self.assertFalse((target / ".rw-test-helpers").exists())

    def test_explicit_dev_environment_overrides_are_verified(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture(root)
            with mock.patch.object(HELPER, "ROOT", root):
                self.assertEqual(HELPER.dev_profile({}), PROFILE)
                self.assertEqual(HELPER.dev_profile({"CARGO_PROFILE_DEV_DEBUG": "0", "CARGO_PROFILE_DEV_OPT_LEVEL": "1"}),
                                 {**PROFILE, "debuginfo": 0, "opt_level": "1"})
                with self.assertRaises(ValueError):
                    HELPER.dev_profile({"CARGO_PROFILE_DEV_DEBUG_ASSERTIONS": "maybe"})

    def test_malformed_hanging_and_flooding_cargo_is_settled(self):
        for flood in [False, True]:
            with self.subTest(flood=flood), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                target, _ = fixture(root)
                evidence = root / "evidence"
                evidence.mkdir()
                pid_file = root / "cargo.pid"
                owned_run = HELPER.run_sample
                def compiler(_command, **options):
                    self.assertEqual(options["timeout"], 7200)
                    options["timeout"] = 1
                    options["output_limit"] = 4096
                    body = (f"import os,pathlib,time; pathlib.Path({str(pid_file)!r}).write_text(str(os.getpid())); "
                            f"print({'x' * 8192 if flood else 'malformed cargo json'!r}, flush=True); time.sleep(30)")
                    return owned_run([sys.executable, "-c", body], **options)
                with mock.patch.object(HELPER, "ROOT", root), mock.patch.dict(os.environ, {}, clear=True), \
                        mock.patch.object(HELPER.native_candidate, "source_identity", return_value={"source": "one"}), \
                        mock.patch.object(HELPER.native_candidate, "configuration_fingerprints", return_value={}), \
                        mock.patch.object(HELPER.tempfile, "mkdtemp", return_value=str(evidence)), \
                        mock.patch.object(HELPER, "run_sample", side_effect=compiler), \
                        self.assertRaises(ValueError if flood else TimeoutError):
                    HELPER.build()
                self.assertFalse((target / ".rw-test-helpers").exists())
                self.assertGreater((evidence / "drain.log").stat().st_size, 0)
                with self.assertRaises(ProcessLookupError):
                    os.kill(int(pid_file.read_text()), 0)


if __name__ == "__main__":
    unittest.main()
