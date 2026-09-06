from __future__ import annotations

import importlib.util
import json
import hashlib
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import artifact_bundle
import native_candidate
import native_profile
from release_contract import load_contract

spec = importlib.util.spec_from_file_location("candidate_package", SCRIPTS / "package-release.py")
packager = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packager)

spec = importlib.util.spec_from_file_location("candidate_builder", SCRIPTS / "build-native-candidate.py")
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)


class NativeCandidateFixture:

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name) / "repo"
        self.repo.mkdir()
        for name in ("packages/tui/scripts/native-lifetime-probe.ts", "contracts/opentui-native.json", "patches/opentui/reclaim-native-owners.patch", "Cargo.toml", "rust-toolchain.toml", ".bun-version", "contracts/release-contract.json",
                     "packages/tui/package.json", "packages/plugin-sdk/package.json", "packages/plugin-host/package.json", "packages/js-host/package.json"):
            target = self.repo / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(SCRIPTS.parent / name, target)
        (self.repo / ".gitignore").write_text("/target/\n**/__pycache__/\n")
        self.git("init", "-q")
        self.git("add", ".")
        self.git("-c", "user.name=Candidate test", "-c", "user.email=candidate@example.invalid",
                 "commit", "-qm", "fixture")
        self.root = Path(self.temporary.name) / "candidate"
        self.root.mkdir()
        self.contract = load_contract(self.repo / "contracts/release-contract.json")
        self.platform = self.contract.resolve_platform(native_candidate.host_platform.system(), native_candidate.host_platform.machine())
        rust, bun = native_candidate.pinned_toolchains(self.repo)
        self.identity = {
            "source": native_candidate.source_identity(self.repo), "platform": self.platform.id,
            "target": f"{self.platform.rust_arch}-apple-darwin" if self.platform.system == "Darwin" else f"{self.platform.rust_arch}-unknown-linux-gnu",
            "version": "0.1.4", "toolchains": {"rust": f"rustc {rust} (fixture)", "bun": bun + "+fixture", "opentui_native": native_candidate.opentui_native.identity(self.repo, self.platform.id)},
            "profile": {"name": "release", "debug": 0, "opt_level": "3" if self.platform.system == "Darwin" else "s", "environment": {}},
            "cargo_configuration": {},
        }
        self.identity["profile"].update(native_profile.settings(self.identity["target"]))
        self.identity["toolchains"]["rust"] += "\nhost: " + self.identity["target"]
        self.stage = self.root / self.contract.archive_root(self.identity["version"], self.platform.id)
        for member in self.platform.archive_members:
            path = self.stage / member.path
            path.parent.mkdir(parents=True, exist_ok=True)
            content = member.id.encode()
            if member.id == "wasm_host_identity":
                content = json.dumps({"bytes": len(b"wasm_host"), "sha256": hashlib.sha256(b"wasm_host").hexdigest()}).encode()
            path.write_bytes(content)
            path.chmod(member.mode)
        self.archive = self.root / (self.stage.name + ".tar.gz")
        packager.package(self.stage, self.archive, 1700000000)
        self.publish()

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.repo, stderr=subprocess.DEVNULL)

    def publish(self):
        paths = {member.id: self.stage.name + "/" + member.path for member in self.platform.archive_members}
        paths["archive"] = self.archive.name
        receipt = {"schema_version": 1, "identity": self.identity,
                   "identity_sha256": native_candidate.identity_key(self.identity), "origin": {},
                   "components": {name: {"path": path, "bytes": (self.root / path).stat().st_size,
                                          "sha256": native_candidate.hash_file(self.root / path)}
                                  for name, path in paths.items()}}
        (self.root / native_candidate.RECEIPT).write_text(json.dumps(receipt))
        (self.root / artifact_bundle.MANIFEST).write_text(json.dumps(artifact_bundle.document(
            self.root, self.identity["source"]["commit"], self.platform.id)))


class NativeCandidateTests(NativeCandidateFixture, unittest.TestCase):
    def test_verified_candidate_paths_follow_the_release_contract(self):
        result = native_candidate.verify(self.root, self.repo, expected_identity=self.identity)
        self.assertEqual(result["identity"], self.identity)
        self.assertEqual(native_candidate.component_path(self.root, self.repo, "archive"), self.archive)

    def test_changed_source_and_untracked_source_invalidate_candidate(self):
        (self.repo / "new-source.rs").write_text("fn new_source() {}")
        with self.assertRaisesRegex(ValueError, "source or native platform"):
            native_candidate.verify(self.root, self.repo)
        (self.repo / "new-source.rs").unlink()
        (self.repo / "Cargo.toml").write_text("changed")
        with self.assertRaisesRegex(ValueError, "source or native platform"):
            native_candidate.verify(self.root, self.repo)

    def test_rebound_foreign_target_is_rejected(self):
        self.identity["target"] = "wasm32-unknown-unknown"
        self.publish()
        with self.assertRaisesRegex(ValueError, "host or target"):
            native_candidate.verify(self.root, self.repo)

    def test_ignored_build_output_does_not_change_source_identity(self):
        (self.repo / "target").mkdir()
        (self.repo / "target" / "output").write_bytes(b"build output")
        self.assertEqual(native_candidate.source_identity(self.repo), self.identity["source"])

    def test_changed_component_is_rejected_before_a_gate_receives_its_path(self):
        member = next(member for member in self.platform.archive_members if member.id == "engine")
        (self.stage / member.path).write_bytes(b"different")
        with self.assertRaisesRegex(ValueError, "identity or contents"):
            native_candidate.component_path(self.root, self.repo, "engine")

    def test_archive_and_staged_components_must_be_the_same_build(self):
        member = next(member for member in self.platform.archive_members if member.id == "engine")
        (self.stage / member.path).write_bytes(b"second build")
        self.publish()
        with self.assertRaisesRegex(ValueError, "archive differs from staged"):
            native_candidate.verify(self.root, self.repo)

    def test_profile_and_compiler_identity_cannot_be_reused_for_another_tuple(self):
        other = dict(self.identity, target="another-target")
        with self.assertRaisesRegex(ValueError, "tuple differs"):
            native_candidate.verify(self.root, self.repo, expected_identity=other)
        self.identity["profile"]["debug"] = 1
        self.publish()
        with self.assertRaisesRegex(ValueError, "release profile"):
            native_candidate.verify(self.root, self.repo)

    def test_missing_or_changed_native_flags_cannot_pass_receipt_verification(self):
        self.identity["profile"].pop("rustflags")
        self.publish()
        with self.assertRaisesRegex(ValueError, "code generation"):
            native_candidate.verify(self.root, self.repo)
        self.identity["profile"]["rustflags"] = ["-C", "force-unwind-tables=yes"]
        self.publish()
        with self.assertRaisesRegex(ValueError, "code generation"):
            native_candidate.verify(self.root, self.repo)

    def test_symlinked_receipt_is_rejected(self):
        receipt = self.root / native_candidate.RECEIPT
        saved = self.root.parent / "receipt"
        receipt.rename(saved)
        receipt.symlink_to(saved)
        with self.assertRaisesRegex(ValueError, "regular file"):
            native_candidate.verify(self.root, self.repo)

    def test_component_mode_does_not_enter_a_shell_command(self):
        with self.assertRaisesRegex(ValueError, "unknown candidate component"):
            native_candidate.component_path(self.root, self.repo, "engine; echo injected")


class NativeCandidatePublicationTests(NativeCandidateFixture, unittest.TestCase):
    def test_existing_verified_tuple_is_reused_without_compiling(self):
        base = self.root.parent / "cache"
        base.mkdir()
        destination = base / native_candidate.identity_key(self.identity)
        self.root.rename(destination)
        with patch.object(builder, "REPO", self.repo), \
             patch.object(native_candidate, "build_identity", return_value=self.identity), \
             patch.object(builder, "run", side_effect=AssertionError("reuse cannot compile")), \
             patch.object(builder.ci_inventory, "install", side_effect=AssertionError("reuse cannot install")):
            self.assertEqual(builder.build(base, self.repo / "target"), destination)

    def test_corrupt_existing_tuple_fails_without_rebuild_or_replacement(self):
        base = self.root.parent / "cache"
        base.mkdir()
        destination = base / native_candidate.identity_key(self.identity)
        self.root.rename(destination)
        engine = destination / self.stage.name / "bin/rw"
        engine.write_bytes(b"corrupt")
        with patch.object(builder, "REPO", self.repo), \
             patch.object(native_candidate, "build_identity", return_value=self.identity), \
             patch.object(builder, "run", side_effect=AssertionError("corrupt cache cannot rebuild silently")):
            with self.assertRaises(ValueError):
                builder.build(base, self.repo / "target")
        self.assertEqual(engine.read_bytes(), b"corrupt")

    def test_failed_compilation_never_publishes_a_candidate(self):
        base = self.root.parent / "cache"
        with patch.object(builder, "REPO", self.repo), \
             patch.object(native_candidate, "build_identity", return_value=self.identity), \
             patch.object(builder, "run", side_effect=RuntimeError("compiler failed")):
            with self.assertRaisesRegex(RuntimeError, "compiler failed"):
                builder.build(base, self.repo / "target")
        self.assertEqual([path.name for path in base.iterdir()], [".build.lock"])
