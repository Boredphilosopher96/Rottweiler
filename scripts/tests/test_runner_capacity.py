import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location("runner_capacity", Path(__file__).resolve().parents[1] / "check-runner-capacity.py")
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class RunnerCapacityTests(unittest.TestCase):
    def test_absent_offline_busy_and_eligible_are_distinct(self):
        self.assertTrue(all(item["state"] == "absent" for item in MODULE.capacity([]).values()))
        runner = {"labels": [{"name": name} for name in MODULE.REQUIRED["darwin-arm64"]], "status": "offline", "busy": False}
        self.assertEqual(MODULE.capacity([runner])["darwin-arm64"]["state"], "offline")
        runner.update(status="online", busy=True)
        self.assertEqual(MODULE.capacity([runner])["darwin-arm64"]["state"], "busy")
        runner["busy"] = False
        self.assertEqual(MODULE.capacity([runner])["darwin-arm64"]["state"], "ready")
        self.assertEqual(MODULE.capacity([runner])["linux-x86_64"]["state"], "absent")
        runner["labels"] = [{"name": "self-hosted"}]
        self.assertEqual(MODULE.capacity([runner])["darwin-arm64"]["state"], "absent")
