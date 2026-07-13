#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).parents[1] / "run-soak.py"
SPEC = importlib.util.spec_from_file_location("run_soak", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
SOAK = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = SOAK
SPEC.loader.exec_module(SOAK)


class SoakHarnessTests(unittest.TestCase):
    def test_release_workflow_uses_only_public_rw_and_sibling_discovery(self) -> None:
        release = (MODULE_PATH.parents[1] / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        soak = release.split("  release-soak:", 1)[1].split(
            "  wsl2-acceptance:", 1
        )[0]
        self.assertIn('--rw "$ROTTWEILER_SOAK_RW"', soak)
        self.assertNotIn("ROTTWEILER_SOAK_TUI", soak)
        self.assertNotIn("--tui", soak)

    def test_workload_streams_and_schedules_tool_and_compaction_paths(self) -> None:
        steps, scripts = SOAK.build_workload(10, compact_every=4, tool_every=3)

        self.assertEqual(len(steps), 10)
        self.assertEqual([step.kind for step in steps].count("compact"), 2)
        self.assertEqual([step.kind for step in steps].count("tool"), 3)
        self.assertGreater(len(scripts), len(steps))
        self.assertTrue(
            any(
                sum(event["type"] == "text_delta" for event in call) >= 4
                for call in scripts
            )
        )
        tool_call = next(
            call for call in scripts if any(event["type"] == "tool_call_start" for event in call)
        )
        self.assertEqual(tool_call[0]["name"], "read")
        self.assertEqual(tool_call[1]["arguments"], {"path": "soak.txt"})
        self.assertTrue(all(step.marker not in step.prompt for step in steps))
        self.assertEqual(SOAK.TERMINAL_SUBMIT, b"\r")

    def test_process_tree_tracks_combined_rss_and_named_descendants(self) -> None:
        rows = SOAK.parse_process_table(
            "10 1 100 /tmp/rw\n"
            "11 10 200 /tmp/rw serve --max-turns 32\n"
            "12 10 300 /tmp/rottweiler-tui\n"
            "13 11 400 helper\n"
            "99 1 999 unrelated\n"
        )

        self.assertEqual(SOAK.descendants(rows, 10), {10, 11, 12, 13})
        self.assertEqual(
            SOAK.find_descendant(rows, 10, Path("/tmp/rw"), " serve "), 11
        )
        self.assertEqual(
            SOAK.find_descendant(rows, 10, Path("/tmp/rottweiler-tui")), 12
        )

    def test_event_probe_reads_only_growth_and_remembers_persisted_marker(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            sessions = Path(temporary)
            log = sessions / "session-1" / "events.jsonl"
            log.parent.mkdir()
            log.write_text('{"event":{"type":"session_created"}}\n', encoding="utf-8")
            probe = SOAK.EventLogProbe(sessions)

            self.assertFalse(probe.poll("SOAK_STEP_000001_DONE"))
            first_bytes = probe.bytes_observed
            with log.open("a", encoding="utf-8") as handle:
                handle.write(
                    '{"event":{"type":"text_delta","text":"SOAK_STEP_000001_DONE"}}\n'
                )
            self.assertTrue(probe.poll("SOAK_STEP_000001_DONE"))
            self.assertTrue(probe.marker_persisted("SOAK_STEP_000001_DONE"))
            self.assertGreater(probe.bytes_observed, first_bytes)
            observed = probe.bytes_observed
            self.assertTrue(probe.poll("SOAK_STEP_000001_DONE"))
            self.assertEqual(probe.bytes_observed, observed)
            self.assertGreater(probe.durable_bytes(), 0)
            log.write_text("durable marker removed\n", encoding="utf-8")
            self.assertFalse(probe.marker_persisted("SOAK_STEP_000001_DONE"))


if __name__ == "__main__":
    unittest.main()
