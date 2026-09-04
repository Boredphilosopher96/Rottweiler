from __future__ import annotations

import copy
import importlib.util
from pathlib import Path
import unittest

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


if __name__ == "__main__":
    unittest.main()
