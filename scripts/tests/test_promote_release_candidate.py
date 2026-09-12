from __future__ import annotations

import base64
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import sys
import tomllib
import unittest


SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
from scripts.tests import test_native_candidate


spec = importlib.util.spec_from_file_location(
    "promote_release_candidate", SCRIPTS / "promote-release-candidate.py"
)
promotion = importlib.util.module_from_spec(spec)
spec.loader.exec_module(promotion)


class PromotionFixture(test_native_candidate.NativeCandidateFixture):
    def setUp(self):
        super().setUp()
        root_chain = self.repo / "release/update/root-chain.json"
        root_chain.parent.mkdir(parents=True)
        shutil.copyfile(SCRIPTS.parent / "release/update/root-chain.json", root_chain)
        self.git("add", "release/update/root-chain.json")
        self.git(
            "-c", "user.name=Candidate test", "-c", "user.email=candidate@example.invalid",
            "commit", "-qm", "add updater root",
        )

        self.version = tomllib.loads((self.repo / "Cargo.toml").read_text())["workspace"]["package"]["version"]
        if self.identity["version"] != self.version:
            self.archive.unlink()
            new_stage = self.root / self.contract.archive_root(self.version, self.platform.id)
            self.stage.rename(new_stage)
            self.stage = new_stage
            self.archive = self.root / f"{self.stage.name}.tar.gz"
            self.identity["version"] = self.version
            test_native_candidate.packager.package(self.stage, self.archive, 1700000000)

        root = promotion.latest_root_role(self.repo)
        keys = {key_id: root["keys"][key_id] for key_id in root["root_key_ids"]}
        self.identity["source"] = test_native_candidate.native_candidate.source_identity(self.repo)
        self.environment = {
            "GITHUB_REF": f"refs/tags/v{self.version}",
            "GITHUB_SHA": self.identity["source"]["commit"],
            "ROTTWEILER_UPDATE_ROOT_VERSION": str(root["version"]),
            "ROTTWEILER_UPDATE_ROOT_THRESHOLD": str(root["root_threshold"]),
            "ROTTWEILER_UPDATE_ROOT_KEYS_JSON": json.dumps(keys, sort_keys=True),
            "ROTTWEILER_UPDATE_BASE_URL": "https://updates.example.invalid/",
        }
        self.identity["profile"]["environment"] = {
            name: hashlib.sha256(value.encode()).hexdigest()
            for name, value in self.environment.items()
            if name in promotion.UPDATE_ENVIRONMENT
        }
        self.publish()
        self.github_output = self.root.parent / "github-output"


class PromoteReleaseCandidateTests(PromotionFixture, unittest.TestCase):
    def test_promotes_verified_archive_without_rebuilding(self):
        archive = promotion.promote(
            self.root, self.version, self.platform.id, self.github_output,
            repo=self.repo, environment=self.environment,
        )
        self.assertEqual(archive, self.archive.absolute())
        self.assertEqual(self.github_output.read_text(), f"path={self.archive.absolute()}\n")

    def test_changed_checkout_source_is_rejected(self):
        (self.repo / "untracked-source.rs").write_text("fn changed() {}")
        with self.assertRaisesRegex(ValueError, "source or native platform"):
            promotion.promote(
                self.root, self.version, self.platform.id, self.github_output,
                repo=self.repo, environment=self.environment,
            )
        self.assertFalse(self.github_output.exists())

    def test_candidate_must_match_the_release_event_sha(self):
        for github_sha, error in ((None, "requires GITHUB_SHA"), ("0" * 40, "differs from GITHUB_SHA")):
            with self.subTest(github_sha=github_sha):
                environment = dict(self.environment)
                if github_sha is None:
                    environment.pop("GITHUB_SHA")
                else:
                    environment["GITHUB_SHA"] = github_sha
                with self.assertRaisesRegex(ValueError, error):
                    promotion.promote(
                        self.root, self.version, self.platform.id, self.github_output,
                        repo=self.repo, environment=environment,
                    )
                self.assertFalse(self.github_output.exists())

    def test_candidate_must_bind_the_configured_update_environment(self):
        environment = dict(self.environment)
        environment["ROTTWEILER_UPDATE_BASE_URL"] = "https://other.example.invalid/"
        with self.assertRaisesRegex(ValueError, "build environment differs.*BASE_URL"):
            promotion.promote(
                self.root, self.version, self.platform.id, self.github_output,
                repo=self.repo, environment=environment,
            )

    def test_requested_version_and_platform_must_match(self):
        with self.assertRaisesRegex(ValueError, "version does not match the workspace"):
            promotion.promote(
                self.root, "9.9.9", self.platform.id, self.github_output,
                repo=self.repo, environment=self.environment,
            )
        other = next(platform.id for platform in self.contract.platforms if platform.id != self.platform.id)
        with self.assertRaisesRegex(ValueError, "candidate platform differs"):
            promotion.promote(
                self.root, self.version, other, self.github_output,
                repo=self.repo, environment=self.environment,
            )

    def test_release_must_run_from_the_exact_version_tag(self):
        environment = dict(self.environment, GITHUB_REF="refs/heads/main")
        with self.assertRaisesRegex(ValueError, "exact version tag"):
            promotion.promote(
                self.root, self.version, self.platform.id, self.github_output,
                repo=self.repo, environment=environment,
            )

    def test_configured_updater_values_must_match_the_signed_root(self):
        cases = (
            ("ROTTWEILER_UPDATE_ROOT_VERSION", "99", "root version"),
            ("ROTTWEILER_UPDATE_ROOT_THRESHOLD", "99", "root threshold"),
            ("ROTTWEILER_UPDATE_ROOT_KEYS_JSON", "{}", "root keys"),
            ("ROTTWEILER_UPDATE_BASE_URL", "https://updates.example.invalid", "must end with"),
        )
        for name, value, error in cases:
            with self.subTest(name=name):
                environment = dict(self.environment, **{name: value})
                with self.assertRaisesRegex(ValueError, error):
                    promotion.promote(
                        self.root, self.version, self.platform.id, self.github_output,
                        repo=self.repo, environment=environment,
                    )

    def test_updater_public_keys_must_be_canonical_32_byte_base64(self):
        root = promotion.latest_root_role(self.repo)
        key_id = root["root_key_ids"][0]
        root["keys"][key_id] = base64.b64encode(b"short").decode()
        payload = base64.b64encode(json.dumps(root).encode()).decode()
        root_chain = json.loads((self.repo / "release/update/root-chain.json").read_text())
        envelope = json.loads(base64.b64decode(root_chain["roots"][-1]["envelope"]))
        envelope["payload"] = payload
        root_chain["roots"][-1]["envelope"] = base64.b64encode(json.dumps(envelope).encode()).decode()
        (self.repo / "release/update/root-chain.json").write_text(json.dumps(root_chain))
        keys = {key: root["keys"][key] for key in root["root_key_ids"]}
        values = promotion.required_environment(dict(
            self.environment, ROTTWEILER_UPDATE_ROOT_KEYS_JSON=json.dumps(keys)
        ))
        with self.assertRaisesRegex(ValueError, "canonical 32-byte base64"):
            promotion.validate_update_configuration(self.repo, values)


if __name__ == "__main__":
    unittest.main()
