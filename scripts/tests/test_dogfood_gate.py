from __future__ import annotations

import datetime as dt
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "check-dogfood-gate.py"
SPEC = importlib.util.spec_from_file_location("dogfood_gate", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class DogfoodGateTests(unittest.TestCase):
    def record(self, date: dt.date, p0: int = 0) -> dict[str, object]:
        return {
            "date": date.isoformat(),
            "commit": "0123456789abcdef",
            "session_ids": [f"session-{date.isoformat()}"],
            "p0_incidents": p0,
        }

    def test_exact_fourteen_day_window_passes(self) -> None:
        through = dt.date(2026, 7, 10)
        records = [
            self.record(through - dt.timedelta(days=offset))
            for offset in reversed(range(14))
        ]
        report = MODULE.check_gate(records, through)
        self.assertEqual(report["consecutive_days"], 14)

    def test_gap_or_p0_fails(self) -> None:
        through = dt.date(2026, 7, 10)
        records = [
            self.record(through - dt.timedelta(days=offset))
            for offset in reversed(range(14))
        ]
        records.pop(4)
        with self.assertRaisesRegex(ValueError, "consecutive"):
            MODULE.check_gate(records, through)
        records.append(self.record(through, 1))
        with self.assertRaisesRegex(ValueError, "P0"):
            MODULE.check_gate(records, through)

    def test_reader_rejects_unknown_schema(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "ledger.jsonl"
            path.write_text(json.dumps({"date": "2026-07-10", "extra": True}) + "\n")
            with self.assertRaisesRegex(ValueError, "schema"):
                MODULE.read_ledger(path)


if __name__ == "__main__":
    unittest.main()
