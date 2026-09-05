#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import contextlib
import io
import json
import sys
import tempfile
import unittest
from unittest import mock
from pathlib import Path


MODULE_PATH = Path(__file__).parents[1] / "run-soak.py"
SPEC = importlib.util.spec_from_file_location("run_soak", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
SOAK = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = SOAK
SPEC.loader.exec_module(SOAK)


class SoakHarnessTests(unittest.TestCase):
    def test_ci_checkpoints_are_remote_metadata_without_terminal_or_transcript_data(self) -> None:
        stderr = io.StringIO()
        with mock.patch.dict(SOAK.os.environ, {"GITHUB_ACTIONS": "true", "ROTTWEILER_CANDIDATE_SHA": "a" * 40}), contextlib.redirect_stderr(stderr):
            progress = SOAK.SoakProgress()
            progress.checkpoint(turns_completed=12, terminal_tail="private terminal", durable_sessions=["private message"])
            progress.checkpoint(turns_completed=13)
        records = stderr.getvalue().splitlines()
        self.assertEqual(len(records), 1)
        record = json.loads(records[0])
        self.assertEqual(record["turns_completed"], 12)
        self.assertEqual(record["source_sha"], "a" * 40)
        self.assertNotIn("private", stderr.getvalue())

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
        for index, step in enumerate(steps, start=1):
            self.assertIn(f"SOAK_INPUT_{index:06d}", step.prompt)
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
            log = sessions / "session-1" / "journal" / "active.jsonl"
            log.parent.mkdir(parents=True)
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

    def test_event_probe_preserves_offsets_and_markers_across_rotation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            sessions = Path(temporary)
            log = sessions / "session-1" / "journal" / "active.jsonl"
            log.parent.mkdir(parents=True)
            log.write_text('{"event":{"type":"text_delta","text":"SOAK_STEP_000001_DONE"}}\n')
            probe = SOAK.EventLogProbe(sessions)
            self.assertTrue(probe.poll("SOAK_STEP_000001_DONE"))
            before = probe.bytes_observed
            sealed = log.with_name(f"{0:020}-{1:020}-{log.stat().st_size:020}-{'a' * 64}.jsonl")
            log.rename(sealed)
            log.write_text('{"event":{"type":"text_delta","text":"SOAK_STEP_000002_DONE"}}\n')
            self.assertTrue(probe.poll("SOAK_STEP_000002_DONE"))
            self.assertTrue(probe.marker_persisted("SOAK_STEP_000001_DONE"))
            self.assertEqual(probe.event_count("text_delta"), 2)
            self.assertEqual(probe.bytes_observed, before + log.stat().st_size)

    def test_event_probe_tracks_input_acceptance_and_compaction_boundaries(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            sessions = Path(temporary)
            log = sessions / "session-1" / "journal" / "active.jsonl"
            log.parent.mkdir(parents=True)
            probe = SOAK.EventLogProbe(sessions)
            log.write_text(
                '{"event":{"type":"user_message_accepted",'
                '"content":"SOAK_INPUT_000001"}}\n'
                '{"event":{"type":"compaction_started"}}\n',
                encoding="utf-8",
            )

            probe.poll()
            self.assertTrue(probe.saw("SOAK_INPUT_000001"))
            self.assertEqual(probe.event_count("user_message_accepted"), 1)
            self.assertEqual(probe.event_count("compaction_started"), 1)
            with log.open("a", encoding="utf-8") as handle:
                handle.write('{"event":{"type":"compaction_finished"}}\n')
            probe.poll()
            self.assertEqual(probe.event_count("compaction_started"), 1)
            self.assertEqual(probe.event_count("compaction_finished"), 1)

    def test_failure_result_is_written_for_artifact_retention(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "soak-result.json"
            result = SOAK.failure_result(RuntimeError("accepted compaction stalled"))
            SOAK.write_result(output, result)

            self.assertEqual(
                json.loads(output.read_text(encoding="utf-8")),
                {
                    "error": "accepted compaction stalled",
                    "error_type": "RuntimeError",
                    "schema_version": 1,
                    "status": "fail",
                },
            )

    def test_memory_failure_retains_process_and_workload_diagnostics(self) -> None:
        error = SOAK.SoakFailure(
            "combined engine/TUI RSS 700 exceeds limit 600",
            {
                "max_rss_bytes": 700,
                "process_rss": [
                    {"executable": "rw", "pid": 10, "rss_bytes": 300},
                    {"executable": "rottweiler-tui", "pid": 11, "rss_bytes": 400},
                ],
                "rss_limit_bytes": 600,
                "turns_completed": 20,
            },
        )

        result = SOAK.failure_result(error)

        self.assertEqual(result["status"], "fail")
        self.assertEqual(result["max_rss_bytes"], 700)
        self.assertEqual(result["rss_limit_bytes"], 600)
        self.assertEqual(result["turns_completed"], 20)
        self.assertEqual(len(result["process_rss"]), 2)

    def test_any_workload_exception_preserves_last_progress_atomically(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "soak.json"

            def fail_after_progress(*args: object) -> None:
                progress = args[-1]
                progress.checkpoint(
                    phase="input_acceptance", turns_submitted=341, turns_accepted=340,
                    turns_completed=340, last_accepted_marker="SOAK_STEP_000340_DONE",
                    last_completed_marker="SOAK_STEP_000340_DONE", supervisor_pid=100,
                    process_rss=[{"pid": 101, "parent_pid": 100, "executable": "rw", "rss_bytes": 50}],
                )
                checkpoint = json.loads(output.read_text())
                self.assertEqual(checkpoint["status"], "running")
                raise RuntimeError("input stalled: access_token=soak-private-canary")

            with mock.patch.object(SOAK, "_run_soak", side_effect=fail_after_progress):
                with self.assertRaises(SOAK.SoakFailure):
                    SOAK.run_soak(Path("rw"), None, 10, 1, 600, progress_path=output)

            result = json.loads(output.read_text())
            self.assertEqual(result["status"], "fail")
            self.assertEqual(result["error_type"], "RuntimeError")
            self.assertEqual(result["phase"], "input_acceptance")
            self.assertEqual(result["turns_completed"], 340)
            self.assertEqual(result["turns_accepted"], 340)
            self.assertEqual(result["turns_submitted"], 341)
            self.assertEqual(result["supervisor_pid"], 100)
            self.assertEqual(result["process_rss"][0]["pid"], 101)
            self.assertIn("duration_seconds", result)
            self.assertNotIn("soak-private-canary", output.read_text())
            self.assertEqual(list(output.parent.glob(".soak.json.*")), [])

    def test_setup_failure_and_interruption_also_write_diagnostics(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "soak.json"
            with self.assertRaises(SOAK.SoakFailure):
                SOAK.run_soak(Path(temporary) / "missing", None, 10, 1, 600, progress_path=output)
            result = json.loads(output.read_text())
            self.assertEqual(result["phase"], "setup")
            self.assertEqual(result["turns_submitted"], 0)
            with mock.patch.object(SOAK, "_run_soak", side_effect=KeyboardInterrupt):
                with self.assertRaises(SOAK.SoakFailure):
                    SOAK.run_soak(Path("rw"), None, 10, 1, 600, progress_path=output)
            self.assertEqual(json.loads(output.read_text())["error_type"], "KeyboardInterrupt")

    def test_failed_atomic_write_preserves_previous_progress(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "soak.json"
            SOAK.write_result(output, {"status": "running", "turns_completed": 5})
            with mock.patch.object(SOAK.os, "replace", side_effect=OSError("disk unavailable")):
                with self.assertRaises(OSError):
                    SOAK.write_result(output, {"status": "running", "turns_completed": 6})
            self.assertEqual(json.loads(output.read_text())["turns_completed"], 5)
            self.assertEqual(list(output.parent.glob(".soak.json.*")), [])

    def test_event_diagnostics_keep_identities_not_payloads_across_split_records(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "session-1" / "journal" / "active.jsonl"
            path.parent.mkdir(parents=True)
            raw = json.dumps({"sequence": "9", "event": {
                "type": "text_delta", "turn_id": "2", "text": "payload-private-canary",
                "meta": {"session_id": "session-1", "sequence_id": "9", "caused_by": "req-1"},
            }}).encode() + b"\n"
            probe = SOAK.EventLogProbe(Path(temporary))
            path.write_bytes(raw[:60])
            probe.poll()
            self.assertNotIn("sequence_id", probe.diagnostics()[0])
            with path.open("ab") as handle:
                handle.write(raw[60:])
            probe.poll()
            result = probe.diagnostics()[0]
            self.assertEqual(result["session_id"], "session-1")
            self.assertEqual(result["sequence_id"], "9")
            self.assertEqual(result["turn_id"], "2")
            self.assertEqual(result["request_id"], "req-1")
            self.assertNotIn("payload-private-canary", json.dumps(result))

    def test_terminal_diagnostic_redacts_credentials_and_bounds_output(self) -> None:
        value = "x" * 8000 + "\x1b[31mAuthorization: Bearer private-canary access_token=second-canary\x1b[0m"
        result = SOAK.redact_diagnostic(value)
        self.assertLessEqual(len(result), SOAK.MAX_DIAGNOSTIC_CHARS)
        self.assertNotIn("private-canary", result)
        self.assertNotIn("second-canary", result)
        self.assertNotIn("\x1b", result)

    def test_real_pty_waits_for_current_driver_and_preserves_failed_submission(self) -> None:
        # These tiny processes exercise the harness PTY/readiness boundary, not
        # the product. Product lifecycle acceptance still uses the built bundle.
        for ready in (False, True):
            with self.subTest(ready=ready), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                rw = root / "rw"
                tui = root / "rottweiler-tui"
                observed = root / "input.txt"
                output = root / "soak.json"
                tui.write_text(f"#!{sys.executable}\nimport time\ntime.sleep(10)\n")
                rw.write_text(
                    f"#!{sys.executable}\n"
                    "import os, select, subprocess, sys, time\n"
                    f"child = subprocess.Popen([{str(tui)!r}])\n"
                    "try:\n"
                    f"    if {ready!r}:\n"
                    "        print('SOAK_DRIVER_', end='', flush=True)\n"
                    "        time.sleep(0.05)\n"
                    "        print('READY', flush=True)\n"
                    "    until = time.monotonic() + 1.4\n"
                    f"    with open({str(observed)!r}, 'wb') as recorded:\n"
                    "        while time.monotonic() < until:\n"
                    "            if select.select([0], [], [], 0.05)[0]:\n"
                    "                recorded.write(os.read(0, 4096))\n"
                    "                recorded.flush()\n"
                    "finally:\n"
                    "    child.terminate()\n"
                    "    child.wait()\n"
                    "sys.exit(7)\n"
                )
                rw.chmod(0o700)
                tui.chmod(0o700)
                def fixture_descendant(rows, supervisor, executable, required=""):
                    # macOS ps can expose only the interpreter name for scripts.
                    # The fixture's only child is its fake TUI; retain real PIDs.
                    if executable.name == "rottweiler-tui":
                        return next((pid for pid, row in rows.items() if row.parent == supervisor), None)
                    return None

                with mock.patch.object(SOAK, "find_descendant", side_effect=fixture_descendant):
                    with self.assertRaises(SOAK.SoakFailure):
                        SOAK.run_soak(rw, None, 4, 0.1, 600 * 1024 * 1024, progress_path=output)
                result = json.loads(output.read_text())
                self.assertEqual(result["status"], "fail")
                self.assertEqual(result["turns_submitted"], int(ready))
                self.assertEqual(result["turns_accepted"], 0)
                self.assertEqual(result["turns_completed"], 0)
                self.assertEqual(result["driver_ready_count"], int(ready))
                self.assertEqual("SOAK_INPUT_000001" in observed.read_text(), ready)
                self.assertEqual(result["phase"], "input_acceptance" if ready else "driver_readiness")
                self.assertGreater(result["samples"], 0)
                self.assertGreater(result["supervisor_pid"], 0)
                self.assertIsNotNone(result["process_snapshot_age_seconds"])


if __name__ == "__main__":
    unittest.main()
