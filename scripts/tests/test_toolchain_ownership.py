from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "check_toolchain_ownership", ROOT / "scripts/check-toolchain-ownership.py"
)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ToolchainOwnershipTests(unittest.TestCase):
    def fixture(self, root: Path) -> None:
        (root / ".github/workflows").mkdir(parents=True)
        (root / "scripts").mkdir()
        (root / "crates/rw-cli/tests").mkdir(parents=True)
        (root / "crates/rw-sandbox/tests").mkdir(parents=True)
        for package in ("plugin-docs", "plugin-sdk", "tui"):
            directory = root / "packages" / package
            directory.mkdir(parents=True)
            (directory / "package.json").write_text(
                json.dumps({"packageManager": "bun@1.2.3", "engines": {"bun": "1.2.3"}}),
                encoding="utf-8",
            )
        (root / "packages/tui/.bun-version").write_text("1.2.3\n", encoding="utf-8")
        (root / "rust-toolchain.toml").write_text(
            '[toolchain]\nchannel = "1.90.0"\n', encoding="utf-8"
        )
        (root / ".bun-version").write_text("1.2.3\n", encoding="utf-8")
        (root / ".github/workflows/ci.yml").write_text(
            "run: rustup toolchain install 1.90.0 --profile minimal\n"
            "bun-version: 1.2.3\n",
            encoding="utf-8",
        )
        (root / "scripts/wsl-acceptance.sh").write_text(
            "rustup override set 1.90.0\n", encoding="utf-8"
        )
        (root / "crates/rw-cli/tests/m8_release_gate_linux.sh").write_text(
            "image=rust:1.90.0-bookworm\n", encoding="utf-8"
        )
        (root / "crates/rw-sandbox/tests/linux_security_gate.sh").write_text(
            "image=rust:1.90.0-bookworm\n", encoding="utf-8"
        )
        (root / "scripts/provision-wsl-ci.sh").write_text(
            "bash -s -- bun-v1.2.3\n", encoding="utf-8"
        )
        (root / "README.md").write_text(
            "Source builds require Rust 1.90.0 and Bun 1.2.3.\n", encoding="utf-8"
        )

    def test_accepts_owned_projections_and_distinct_nightly_channel(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            workflow = root / ".github/workflows/ci.yml"
            workflow.write_text(
                workflow.read_text(encoding="utf-8")
                + "run: rustup toolchain install nightly --profile minimal\n",
                encoding="utf-8",
            )
            self.assertEqual(MODULE.validate_repository(root), [])

    def test_rejects_drifted_workflow_and_package_versions(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            (root / ".github/workflows/ci.yml").write_text(
                "run: rustup override set 1.89.0\nbun-version: 1.2.2\n",
                encoding="utf-8",
            )
            package = root / "packages/plugin-sdk/package.json"
            document = json.loads(package.read_text(encoding="utf-8"))
            document["engines"]["bun"] = "1.2.2"
            package.write_text(json.dumps(document), encoding="utf-8")
            failures = MODULE.validate_repository(root)
            self.assertTrue(any("Rust 1.89.0" in failure for failure in failures))
            self.assertTrue(any("Bun 1.2.2" in failure for failure in failures))
            self.assertTrue(any("plugin-sdk" in failure for failure in failures))

    def test_rejects_non_exact_owner_versions(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            (root / ".bun-version").write_text("latest\n", encoding="utf-8")
            self.assertIn("exact semantic version", MODULE.validate_repository(root)[0])


if __name__ == "__main__":
    unittest.main()
