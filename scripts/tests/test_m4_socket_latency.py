"""Prove direct replies never substitute for the release gate's SSE measurement."""
from __future__ import annotations

import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).parents[1]))
from m4_gate_support import GateEvidence
from m4_socket_latency import command_reply, correlated_event, measure_socket_channels


class CommandConnection:
    def __init__(self) -> None:
        self.command = None
        self.reads = 0
        self.controls = 0

    def send_request(self, method, path, headers, body) -> None:
        assert method == "POST" and path == "/v1/command"
        self.command = json.loads(body)
        if self.command["type"] == "list_models":
            assert self.command["refresh"] is False

    def read_response(self):
        command = self.command
        if command["type"] == "resume_session":
            self.controls += 1
            reply = {"type": "command", "outcome": {"type": "accepted"}}
        else:
            self.reads += 1
            event = {
                "type": "command_descriptors_listed" if command["type"] == "list_commands" else "models_listed",
                "meta": {**command["meta"], "client_id": "bound"},
            }
            if command["type"] == "list_commands":
                event["session_id"] = "session"
            reply = {"type": "read", "outcome": {"type": "accepted"}, "events": [event]}
        return 202, {}, json.dumps(reply).encode()


class EventConnection:
    def __init__(self, commands) -> None:
        self.commands = commands
        self.received = 0

    def next_matching_event(self, request_id, expected_type):
        command = self.commands.command
        assert command["type"] == "resume_session", "a read must never wait on SSE"
        assert command["role"] == "observer"
        self.received += 1
        return {"type": expected_type,
                "meta": {"protocol_version": 1, "client_id": "bound", "request_id": request_id},
                "sessions": [{"session_id": "session"}]}


class SocketLatencyTests(unittest.TestCase):
    def test_direct_reads_and_real_event_lane_have_distinct_samples(self):
        commands = CommandConnection()
        events = EventConnection(commands)
        with tempfile.TemporaryDirectory() as temporary:
            evidence = GateEvidence(Path(temporary) / "evidence.json")
            counter = iter(range(0, 1_000_000_000, 500_000))
            with mock.patch("m4_socket_latency.time.perf_counter_ns", side_effect=lambda: next(counter)):
                result = measure_socket_channels(commands, events, {}, "session", "bound", 3, evidence)
        self.assertEqual(result, {"uds_event_p99_us": 500})
        self.assertEqual(evidence.result["uds_direct_read_p99_us"], 500)
        self.assertEqual(len(evidence.samples["uds_control_event"]), 3)
        self.assertEqual(len(evidence.samples["uds_direct_read"]), 3)
        self.assertEqual(events.received, commands.controls)
        self.assertGreater(commands.reads, events.received)

    def test_two_millisecond_event_ceiling_remains_enforced(self):
        commands = CommandConnection()
        counter = iter(range(0, 1_000_000_000, 2_000_000))
        with mock.patch("m4_socket_latency.time.perf_counter_ns", side_effect=lambda: next(counter)):
            with self.assertRaisesRegex(RuntimeError, "event p99 2.000ms exceeds 2ms"):
                measure_socket_channels(commands, EventConnection(commands), {}, "session", "bound", 3, None)

    def test_reply_classes_and_event_correlation_are_checked_directly(self):
        with self.assertRaises(RuntimeError):
            command_reply(b'{"type":"accepted"}', "read")
        with self.assertRaises(RuntimeError):
            command_reply(b'{"type":"command","outcome":{"type":"accepted"}}', "read")
        with self.assertRaises(RuntimeError):
            command_reply(b'{"type":"read","outcome":{"type":"accepted"},"events":[1]}', "read")
        event = {"type": "models_listed", "meta": {
            "protocol_version": 1, "client_id": "bound", "request_id": "request"}}
        for field, value in [("protocol_version", 2), ("client_id", "spoof"), ("request_id", "old")]:
            with self.assertRaises(RuntimeError):
                correlated_event({**event, "meta": {**event["meta"], field: value}},
                                 "models_listed", "bound", "request")
        with self.assertRaises(RuntimeError):
            correlated_event({**event, "session_id": "foreign"}, "models_listed", "bound", "request", "session")


if __name__ == "__main__":
    unittest.main()
