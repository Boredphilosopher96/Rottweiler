"""Source pins and immutable verified artifacts define portable HEAD toolchains."""
from __future__ import annotations

import json
from pathlib import Path
import shutil
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import homebrew_toolchains as tools

ROOT = Path(__file__).resolve().parents[2]


class HomebrewToolchainTests(unittest.TestCase):
    def fixture(self, directory: str) -> Path:
        root = Path(directory)
        for name in (".bun-version", "rust-toolchain.toml", tools.DIGESTS, "contracts/release-contract.json",
                     *(f"packages/{package}/package.json" for package in ("tui", "plugin-host", "plugin-sdk", "js-host"))):
            target = root / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / name, target)
        return root

    def test_every_release_platform_has_an_immutable_verified_resource(self) -> None:
        resources = tools.verified_resources(ROOT)
        self.assertEqual(len(resources), 4)
        rust, bun = tools.pinned_toolchains(ROOT)
        for system, machine, suffix in (
            ("Darwin", "arm64", "darwin-aarch64"), ("Darwin", "x86_64", "darwin-x64-baseline"),
            ("Linux", "aarch64", "linux-aarch64"), ("Linux", "x86_64", "linux-x64-baseline"),
        ):
            manifest = tools.manifest(ROOT, system, machine)
            self.assertEqual(manifest["rust"], rust)
            self.assertEqual(manifest["bun"]["url"],
                             f"https://github.com/oven-sh/bun/releases/download/bun-v{bun}/bun-{suffix}.zip")
            self.assertRegex(manifest["bun"]["sha256"], r"^[a-f0-9]{64}$")
        with self.assertRaises(ValueError):
            tools.manifest(ROOT, "Windows", "AMD64")

    def test_changed_pin_requires_new_artifact_identities(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = self.fixture(directory)
            (root / ".bun-version").write_text("1.0.0\n")
            for package in ("tui", "plugin-host", "plugin-sdk", "js-host"):
                path = root / f"packages/{package}/package.json"
                data = json.loads(path.read_text())
                data["packageManager"] = "bun@1.0.0"
                path.write_text(json.dumps(data))
            with self.assertRaisesRegex(ValueError, "artifact identities differ from source pins"):
                tools.verified_resources(root)

    def test_missing_extra_and_unhashed_resources_reject_before_provisioning(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = self.fixture(directory)
            path = root / tools.DIGESTS
            document = json.loads(path.read_text())
            original = document["sha256"].copy()
            url = next(iter(original))
            for replacement in ({}, {**original, "https://example.com/unowned.zip": "a" * 64},
                                {**original, url: "latest"}):
                document["sha256"] = replacement
                path.write_text(json.dumps(document))
                with self.assertRaises(ValueError):
                    tools.verified_resources(root)
