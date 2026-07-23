import json
import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


class SessionEventSchemaTests(unittest.TestCase):
    def test_checked_in_envelope_matches_the_implementation_version(self) -> None:
        source = (ROOT / "crates/rw-store/src/session.rs").read_text(encoding="utf-8")
        match = re.search(r"SESSION_EVENT_SCHEMA_VERSION: u16 = (\d+);", source)
        self.assertIsNotNone(match)
        schema = json.loads(
            (ROOT / "protocol/session-event-envelope.schema.json").read_text(encoding="utf-8")
        )
        self.assertEqual(schema["properties"]["schema_version"]["const"], int(match.group(1)))

    def test_envelope_requires_the_public_event_and_decimal_cursor(self) -> None:
        schema = json.loads(
            (ROOT / "protocol/session-event-envelope.schema.json").read_text(encoding="utf-8")
        )
        self.assertNotIn(
            "$id",
            schema,
            "an unpublished absolute id would redirect sibling refs away from the local schema tree",
        )
        self.assertEqual(set(schema["required"]), {"schema_version", "sequence", "event"})
        self.assertEqual(schema["properties"]["sequence"]["$ref"], "#/$defs/SequenceId")
        sequence = schema["$defs"]["SequenceId"]
        self.assertEqual(sequence["type"], "string")
        self.assertEqual(sequence["maxLength"], 20)
        sequence_pattern = re.compile(sequence["pattern"])
        self.assertIsNotNone(sequence_pattern.fullmatch("18446744073709551615"))
        self.assertIsNone(sequence_pattern.fullmatch("18446744073709551616"))
        self.assertIsNone(sequence_pattern.fullmatch("01"))
        event = schema["properties"]["event"]
        self.assertEqual(event["allOf"][0]["$ref"], "schema/engine-event.schema.json")
        durable_meta = event["allOf"][1]
        self.assertEqual(durable_meta["required"], ["meta"])
        self.assertEqual(
            durable_meta["properties"]["meta"]["required"], ["sequence_id"]
        )
        self.assertEqual(
            durable_meta["properties"]["meta"]["properties"]["sequence_id"]["$ref"],
            "#/$defs/SequenceId",
        )

    def test_envelope_excludes_exactly_connection_scoped_engine_events(self) -> None:
        engine_schema = json.loads(
            (ROOT / "protocol/schema/engine-event.schema.json").read_text(encoding="utf-8")
        )
        envelope_schema = json.loads(
            (ROOT / "protocol/session-event-envelope.schema.json").read_text(encoding="utf-8")
        )
        durable_meta = envelope_schema["properties"]["event"]["allOf"][1]
        sequenced_event_types = {
            variant["properties"]["type"]["const"]
            for variant in engine_schema["oneOf"]
            if variant.get("properties", {}).get("meta", {}).get("$ref")
            == "#/$defs/EventMeta"
        }
        # These names deliberately differ from the private PendingEvent variants
        # which produce them. A hand-maintained PendingEvent allowlist previously
        # rejected real records while its mirror-image test remained green.
        self.assertTrue(
            {
                "tool_approval_needed",
                "tool_output_delta",
                "hook_failed",
                "context_usage_updated",
                "budget_status_changed",
            }.issubset(sequenced_event_types)
        )
        acknowledgement_types = {
            variant["properties"]["type"]["const"]
            for variant in engine_schema["oneOf"]
            if variant.get("properties", {}).get("meta", {}).get("$ref")
            == "#/$defs/CommandAckMeta"
        }
        excluded_types = set(
            durable_meta["not"]["properties"]["type"]["enum"]
        )
        self.assertEqual(excluded_types, acknowledgement_types)
        self.assertTrue(sequenced_event_types)
        self.assertTrue(acknowledgement_types)
        self.assertTrue(sequenced_event_types.isdisjoint(excluded_types))

    def test_protocol_index_links_the_durable_schema(self) -> None:
        readme = (ROOT / "protocol/README.md").read_text(encoding="utf-8")
        self.assertIn("session-log.md", readme)
        self.assertIn("session-event-envelope.schema.json", readme)

    def test_documented_record_uses_one_matching_session_sequence(self) -> None:
        document = (ROOT / "protocol/session-log.md").read_text(encoding="utf-8")
        match = re.search(r"```json\n([^\n]+)\n```", document)
        self.assertIsNotNone(match)
        record = json.loads(match.group(1))
        self.assertEqual(record["schema_version"], 1)
        self.assertEqual(record["sequence"], record["event"]["meta"]["sequence_id"])
        self.assertIn("never persisted", document)


if __name__ == "__main__":
    unittest.main()
