"""Artifact denial and unconditional post-run verification, without native work."""
import hashlib
import importlib.util
import json
from pathlib import Path
import platform
import shutil
import sys
import tarfile
import tempfile
import unittest
from unittest import mock

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
from soak_identity import SoakInputs, verify_unchanged
from release_contract import load_contract

REPO = SCRIPTS.parent


class SoakIdentityTests(unittest.TestCase):
    def release(self, root):
        contract = load_contract(REPO / "contracts/release-contract.json")
        target = contract.resolve_platform(platform.system(), platform.machine())
        name = contract.archive_root("0.1.0", target.id)
        stage = root / name
        stage.mkdir(mode=0o755)
        for member in target.archive_members:
            path = stage / member.path
            path.parent.mkdir(exist_ok=True, mode=0o755)
            path.write_bytes(("fixture-" + member.id).encode())
            path.chmod(member.mode)
        helper = stage / "bin/rottweiler-wasm-host"
        (stage / "bin/rottweiler-wasm-host.identity.json").write_text(json.dumps({
            "bytes": helper.stat().st_size,
            "sha256": hashlib.sha256(helper.read_bytes()).hexdigest(),
        }))
        archive = root / f"{name}.tar.gz"
        with tarfile.open(archive, "w:gz") as bundle:
            bundle.add(stage, arcname=name)
        installed = root / "installed"
        shutil.copytree(stage / "bin", installed)
        inputs = SoakInputs(REPO, installed / "rw", None, None, archive, "0.1.0")
        return inputs, installed

    def test_installed_archive_verifies_every_required_member_and_rejects_drift(self):
        with tempfile.TemporaryDirectory() as temporary:
            inputs, installed = self.release(Path(temporary))
            before = inputs.verify()
            self.assertIn("opentui_licenses", before["components"])
            self.assertIn("wasm_host_identity", before["components"])
            self.assertEqual(verify_unchanged(inputs, before), before)
            (installed / "opentui-licenses.txt").write_text("changed license")
            with self.assertRaisesRegex(ValueError, "differs from release"):
                verify_unchanged(inputs, before)

    def test_installed_missing_extra_and_symlink_members_are_rejected(self):
        for mutation in ("missing", "extra", "symlink"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                inputs, installed = self.release(Path(temporary))
                target = installed / "opentui-licenses.txt"
                if mutation == "missing":
                    target.unlink()
                elif mutation == "extra":
                    (installed / "old-plugin-host").write_text("unexpected")
                else:
                    target.unlink()
                    target.symlink_to(installed / "rw")
                with self.assertRaises(ValueError):
                    inputs.verify()

    def test_candidate_uses_required_verifier_and_rejects_another_executable(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            rw, host = root / "rw", root / "rottweiler-js-host"
            rw.write_bytes(b"rw")
            host.write_bytes(b"js")
            (root / "build.json").write_bytes(b"receipt")
            receipt = {"identity": {"source": "exact"}, "components": {
                "engine": {"path": "rw"}, "js_host": {"path": "rottweiler-js-host"},
            }}
            with mock.patch("soak_identity.native_candidate.verify", return_value=receipt) as verify:
                inputs = SoakInputs(REPO, rw, host, root, None, None)
                before = inputs.verify()
                verify.assert_called_once_with(root.resolve(), REPO)
                other = root / "other-rw"
                other.write_bytes(b"rw")
                with self.assertRaisesRegex(ValueError, "differs from verified candidate"):
                    SoakInputs(REPO, other, host, root, None, None).verify()
                (root / "build.json").write_bytes(b"changed receipt")
                with self.assertRaisesRegex(ValueError, "changed during"):
                    verify_unchanged(inputs, before)

    def test_missing_or_ambiguous_identity_refuses_launch(self):
        for candidate, archive in ((None, None), (Path("candidate"), Path("archive"))):
            with self.assertRaisesRegex(ValueError, "exactly one"):
                SoakInputs(REPO, Path("rw"), None, candidate, archive, None).verify()

    def test_main_verifies_after_failure_and_does_not_publish_early_pass(self):
        spec = importlib.util.spec_from_file_location("soak_gate_identity_test", SCRIPTS / "run-soak.py")
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "result.json"
            arguments = ["run-soak", "--rw", "rw", "--candidate", temporary, "--output", str(output)]
            before = {"verified": "before"}
            def after(*_):
                self.assertFalse(output.exists())
                raise ValueError("artifact replaced")
            with mock.patch.object(sys, "argv", arguments), \
                 mock.patch.object(module.SoakInputs, "verify", return_value=before), \
                 mock.patch.object(module, "run_soak", side_effect=RuntimeError("workload failed")), \
                 mock.patch.object(module, "verify_unchanged", side_effect=after) as verify, \
                 mock.patch("builtins.print"), self.assertRaises(SystemExit):
                module.main()
            verify.assert_called_once()
            result = json.loads(output.read_text())
            self.assertEqual(result["status"], "fail")
            self.assertEqual(result["error"], "workload failed")
            self.assertEqual(result["artifact_verification_error"], "artifact replaced")


if __name__ == "__main__":
    unittest.main()
