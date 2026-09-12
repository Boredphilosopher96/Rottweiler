"""Canonical phase proofs, bounded observation, and retained source identity."""
from pathlib import Path
import json
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from soak_journal import EventLogProbe, MAX_RECORD, MAX_POLL_BYTES

MARKER = "SOAK_STEP_000001_DONE"
INPUT = "SOAK_INPUT_000001"


class SoakJournalTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.path = self.root / "session-1" / "journal" / "active.jsonl"
        self.path.parent.mkdir(parents=True)
        self.path.touch()
        self.sequence = 0
        self.probe = EventLogProbe(self.root)

    def append(self, kind, **fields):
        event = {"type": kind, "meta": {"session_id": "session-1", "sequence_id": str(self.sequence), "caused_by": "request-1"}, **fields}
        source = str(self.sequence)
        self.sequence += 1
        with self.path.open("ab") as file:
            file.write(json.dumps({"sequence": source, "event": event}).encode() + b"\n")
        return source

    def start(self, kind="turn"):
        self.probe.begin(MARKER, kind)
        self.append("turn_started", turn_id="1")
        self.append("user_message_accepted", agent_turn="1", content=INPUT, attachments=[])

    def body(self, *, summary=False, turn="1"):
        self.append("conversation_turn_committed", agent_turn=turn, turn={
            "role": "assistant", "blocks": [{"type": "text", "text": MARKER}],
            "meta": {"summary": summary, "synthetic": summary},
        })

    def test_text_marker_is_not_completion_and_terminal_requires_exact_turn(self):
        self.start()
        self.append("text_delta", turn_id="1", text=MARKER)
        self.body()
        self.assertFalse(self.probe.poll(MARKER))
        self.assertTrue(self.probe.saw(INPUT))
        self.append("turn_finished", turn_id="2", status="completed")
        self.assertFalse(self.probe.poll(MARKER))
        self.append("turn_finished", turn_id="1", status="completed")
        self.assertTrue(self.probe.poll(MARKER))
        before = self.probe.bytes_observed
        self.assertTrue(self.probe.poll(MARKER))
        self.assertEqual(before, self.probe.bytes_observed)
        self.assertTrue(self.probe.marker_persisted(MARKER))

    def test_nested_marker_and_discriminator_do_not_fabricate_any_phase(self):
        self.start()
        self.append("tool_call_started", turn_id="1", name="other", args={
            "type": "compaction_finished", "nested": MARKER, "input": INPUT,
        })
        self.assertFalse(self.probe.poll(MARKER))
        self.assertEqual(self.probe.event_count("compaction_finished"), 0)
        self.assertEqual(self.probe.event_count("tool_call_started"), 1)

    def test_tool_completion_requires_success_and_exact_durable_result_reference(self):
        for source in ("wrong", "3"):
            with self.subTest(source=source):
                self.setUp()
                self.start("tool")
                self.append("tool_call_started", turn_id="1", invocation_id="read-1", name="read", args={"path": "soak.txt"})
                finished = self.append("tool_call_finished", turn_id="1", invocation_id="read-1", is_error=False)
                self.assertEqual(finished, "3")
                self.append("conversation_tool_results_committed", agent_turn="1", results=[{"invocation_id": "read-1", "finished_source": source}])
                self.append("text_delta", turn_id="1", text=MARKER)
                self.body()
                self.append("turn_finished", turn_id="1", status="completed")
                if source == "wrong":
                    with self.assertRaisesRegex(RuntimeError, "exact successful"):
                        self.probe.poll(MARKER)
                else:
                    self.assertTrue(self.probe.poll(MARKER))

    def test_failed_turn_and_tool_are_not_success(self):
        self.start("tool")
        self.append("tool_call_started", turn_id="1", invocation_id="read-1", name="read", args={"path": "soak.txt"})
        self.append("tool_call_finished", turn_id="1", invocation_id="read-1", is_error=True)
        with self.assertRaisesRegex(RuntimeError, "read tool failed"):
            self.probe.poll(MARKER)

    def test_manual_compaction_needs_exact_summary_then_finished(self):
        self.probe.begin(MARKER, "compact")
        self.append("compaction_started", reason="manual")
        self.body(summary=True, turn="7")
        self.assertFalse(self.probe.poll(MARKER))
        self.assertEqual(self.probe.event_count("compaction_started"), 1)
        self.append("compaction_finished", summary_turn_id="7")
        self.assertTrue(self.probe.poll(MARKER))
        self.assertTrue(self.probe.marker_persisted(MARKER))

    def test_other_summary_cannot_satisfy_manual_compaction(self):
        self.probe.begin(MARKER, "compact")
        self.append("compaction_started", reason="manual")
        self.body(summary=True, turn="7")
        self.append("compaction_finished", summary_turn_id="8")
        with self.assertRaisesRegex(RuntimeError, "exact committed summary"):
            self.probe.poll(MARKER)

    def test_rotation_preserves_exact_body_and_terminal_record_proof(self):
        self.start()
        self.append("text_delta", turn_id="1", text=MARKER)
        self.body()
        self.probe.poll()
        before = self.probe.bytes_observed
        sealed = self.path.with_name(f"{0:020}-{3:020}-{self.path.stat().st_size:020}-{'a' * 64}.jsonl")
        self.path.rename(sealed)
        self.append("turn_finished", turn_id="1", status="completed")
        self.assertTrue(self.probe.poll(MARKER))
        self.assertEqual(self.probe.bytes_observed, before + self.path.stat().st_size)
        self.assertTrue(self.probe.marker_persisted(MARKER))
        raw = sealed.read_bytes()
        sealed.write_bytes(raw.replace(MARKER.encode(), b"X" * len(MARKER)))
        self.assertFalse(self.probe.marker_persisted(MARKER))

    def test_incomplete_line_never_advances_semantic_phase(self):
        self.start()
        self.probe.poll()
        self.append("text_delta", turn_id="1", text=MARKER)
        raw = self.path.read_bytes()
        self.path.write_bytes(raw[:-1])
        self.probe.poll()
        self.assertEqual(self.probe.event_count("text_delta"), 0)
        with self.path.open("ab") as file:
            file.write(b"\n")
        self.probe.poll()
        self.assertEqual(self.probe.event_count("text_delta"), 1)

    def test_record_bound_and_duplicate_fields_fail_instead_of_skipping(self):
        self.path.write_bytes(b"x" * (MAX_RECORD + 1))
        with self.assertRaisesRegex(RuntimeError, "record exceeds bound"):
            self.probe.poll()
        self.path.write_bytes(b'{"event":{},"event":{}}\n')
        probe = EventLogProbe(self.root)
        with self.assertRaisesRegex(ValueError, "duplicate"):
            probe.poll()

    def test_large_growth_is_split_across_bounded_polls(self):
        for _ in range(80):
            self.append("session_title_updated", title="x" * 65536)
        self.probe.poll()
        self.assertLessEqual(self.probe.bytes_observed, MAX_POLL_BYTES)
        self.assertLess(self.probe.bytes_observed, self.path.stat().st_size)
        self.probe.poll()
        self.assertEqual(self.probe.bytes_observed, self.path.stat().st_size)
        self.assertEqual(self.probe.event_count("session_title_updated"), 80)


if __name__ == "__main__":
    unittest.main()
