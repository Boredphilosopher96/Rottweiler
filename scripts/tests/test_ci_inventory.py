from __future__ import annotations

import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("ci_inventory", ROOT / "scripts/ci_inventory.py")
assert SPEC is not None and SPEC.loader is not None
CI = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CI)


class CiInventoryTests(unittest.TestCase):
    def test_every_non_success_and_missing_result_blocks(self):
        expected = ["test", "tui"]
        self.assertEqual(CI.require_results(expected, {name: {"result": "success"} for name in expected}), [])
        for result in ["failure", "cancelled", "skipped", "timed_out", "unknown", None]:
            with self.subTest(result=result):
                self.assertTrue(CI.require_results(expected, {"test": {"result": "success"}, "tui": {"result": result}}))
        self.assertTrue(CI.require_results(expected, {"test": {"result": "success"}}))
        self.assertTrue(CI.require_results([], {}))

    def test_aggregate_cannot_omit_a_job_or_be_conditional(self):
        workflow = CI.load_workflow(ROOT)
        self.assertEqual(CI.check_workflow(workflow), [])
        omitted = copy.deepcopy(workflow)
        omitted["jobs"]["required"]["needs"].remove("tui-performance-smoke")
        self.assertTrue(CI.check_workflow(omitted))
        conditional = copy.deepcopy(workflow)
        conditional["jobs"]["required"]["if"] = "success()"
        self.assertTrue(CI.check_workflow(conditional))
        filtered = copy.deepcopy(workflow)
        filtered["on"]["pull_request"] = {"paths": ["crates/**"]}
        self.assertTrue(CI.check_workflow(filtered))

    def test_inventory_includes_every_real_package(self):
        self.assertEqual(CI.check_inventory(ROOT), [])
        self.assertIn("packages/plugin-host/package.json", CI.package_manifests(ROOT))

    def test_cold_consumer_install_builds_local_exports_first_in_any_request_order(self):
        for requested in (["plugin-host"], ["plugin-sdk", "plugin-host"]):
            with self.subTest(requested=requested), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                (root / "contracts").mkdir()
                (root / "contracts/package-inventory.json").write_text(json.dumps(CI.inventory(ROOT)))
                (root / ".bun-version").write_text("1.3.14\n")
                for name in ("plugin-sdk", "plugin-host"):
                    directory = root / "packages" / name
                    directory.mkdir(parents=True)
                    (directory / "package.json").write_text(
                        (ROOT / "packages" / name / "package.json").read_text())
                exported = root / "packages/plugin-sdk/dist/index.js"
                calls = []

                def run(command, *, cwd, check):
                    self.assertTrue(check)
                    calls.append((cwd.name, command))
                    if command == ["bun", "run", "build"]:
                        exported.parent.mkdir()
                        exported.write_text("export {};\n")
                    elif cwd.name == "plugin-host":
                        self.assertTrue(exported.is_file(), "consumer must install current built exports")

                with patch.object(CI.subprocess, "check_output", return_value="1.3.14\n"), \
                        patch.object(CI.subprocess, "run", side_effect=run):
                    CI.install(root, requested, build_dependencies=True)
                self.assertEqual(calls, [
                    ("plugin-sdk", ["bun", "install", "--frozen-lockfile"]),
                    ("plugin-sdk", ["bun", "run", "build"]),
                    ("plugin-host", ["bun", "install", "--frozen-lockfile"]),
                ])


if __name__ == "__main__":
    unittest.main()
