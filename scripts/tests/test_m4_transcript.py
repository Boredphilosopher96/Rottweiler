"""The loopback oracle accepts only the fixture's authoritative input selection."""
import copy
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from m4_transcript import fixture_turns


def transcript():
    def event(sequence, kind, **fields):
        return {"sequence": str(sequence), "event": {"type": kind, "meta": {
            "sequence_id": str(sequence), "session_id": "fixture",
        }, **fields}}
    return [event(0, "user_message_accepted", agent_turn="1", content="prompt", attachments=[]),
            event(1, "conversation_input_committed", agent_turn="1", accepted_source="0", selection={"type": "accepted"}),
            event(2, "conversation_turn_committed", agent_turn="1", turn={
                "role": "assistant", "blocks": [{"type": "text", "text": "answer"}], "meta": {},
            })]


class M4TranscriptTests(unittest.TestCase):
    def test_source_only_prompt_and_answer_have_exact_canonical_blocks(self):
        self.assertEqual(fixture_turns(transcript()), [
            {"role": "user", "blocks": [{"type": "text", "text": "prompt"}]},
            {"role": "assistant", "blocks": [{"type": "text", "text": "answer"}]},
        ])
        self.assertEqual(fixture_turns(transcript()[:1]), [])

    def test_missing_reused_foreign_or_substituted_sources_fail(self):
        mutations = [
            lambda rows: rows[1]["event"].update(accepted_source="9"),
            lambda rows: rows.insert(2, copy.deepcopy(rows[1])),
            lambda rows: rows[1]["event"]["meta"].update(session_id="foreign"),
            lambda rows: rows[1]["event"].update(agent_turn="2"),
            lambda rows: rows[0]["event"]["meta"].update(sequence_id="3"),
            lambda rows: rows[1]["event"].update(selection={"type": "transformed", "text": "different"}),
            lambda rows: rows[2]["event"]["turn"].update(role="user"),
        ]
        for mutate in mutations:
            with self.subTest(mutation=mutate):
                rows = transcript()
                mutate(rows)
                with self.assertRaises(RuntimeError):
                    fixture_turns(rows)
