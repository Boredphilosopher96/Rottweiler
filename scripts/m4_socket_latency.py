"""Measure direct read replies and control-result SSE delivery as separate channels."""
from __future__ import annotations

import json
import math
import statistics
import time

from m4_gate_support import GateEvidence, UnixHttpConnection, UnixSseStream


def command_reply(raw: bytes, expected_class: str) -> dict[str, object]:
    value = json.loads(raw)
    if not isinstance(value, dict) or value.get("type") != expected_class:
        raise RuntimeError(f"command reply has the wrong channel class: {value!r}")
    outcome = value.get("outcome")
    if not isinstance(outcome, dict) or outcome.get("type") not in {"accepted", "rejected"}:
        raise RuntimeError("command reply has an invalid typed outcome")
    if outcome["type"] == "rejected" and not isinstance(outcome.get("error"), dict):
        raise RuntimeError("command rejection has no typed error")
    if expected_class == "read" and (
        not isinstance(value.get("events"), list)
        or any(not isinstance(event, dict) for event in value["events"])
    ):
        raise RuntimeError("read reply has invalid query events")
    return value


def correlated_event(
    event: object, expected_type: str, client_id: str, request_id: str,
    session_id: str | None = None,
) -> dict[str, object]:
    if not isinstance(event, dict) or event.get("type") != expected_type:
        raise RuntimeError("reply has the wrong result event")
    meta = event.get("meta")
    if not isinstance(meta, dict) or (
        meta.get("protocol_version") != 1 or meta.get("client_id") != client_id
        or meta.get("request_id") != request_id
    ):
        raise RuntimeError("result did not carry its authenticated request identity")
    if session_id is not None and event.get("session_id") != session_id:
        raise RuntimeError("query result belongs to another session")
    return event


def measure_socket_channels(
    commands: UnixHttpConnection, events: UnixSseStream, headers: dict[str, str],
    session_id: str, client_id: str, samples: int, evidence: GateEvidence | None,
) -> dict[str, int]:
    headers = {**headers, "x-rottweiler-command-lane": "normal"}

    def payload(kind: str, request_id: str) -> bytes:
        value: dict[str, object] = {
            "type": kind,
            "meta": {"protocol_version": 1, "client_id": "transport-spoof", "request_id": request_id},
        }
        if kind in {"list_commands", "resume_session"}:
            value["session_id"] = session_id
        if kind == "resume_session":
            value.update(role="observer", last_seen_sequence=None)
        if kind == "list_models":
            value["refresh"] = False
        return json.dumps(value, separators=(",", ":")).encode("utf-8")

    def read_reply() -> bytes:
        status, _, response = commands.read_response()
        if status != 202:
            raise RuntimeError(f"host command returned HTTP {status}: {response!r}")
        return response

    def query(index: int, phase: str, ready: bool = False) -> tuple[float, dict[str, object]]:
        kind, event_type = ("list_commands", "command_descriptors_listed") if ready or index % 2 == 0 else ("list_models", "models_listed")
        request_id = f"m4-direct-{phase}-{index}"
        encoded = payload(kind, request_id)
        started = time.perf_counter_ns()
        commands.send_request("POST", "/v1/command", headers, encoded)
        response = read_reply()
        elapsed = (time.perf_counter_ns() - started) / 1_000_000
        reply = command_reply(response, "read")
        if reply["outcome"]["type"] == "accepted":
            if len(reply["events"]) != 1:
                raise RuntimeError("query did not return exactly one direct result")
            correlated_event(reply["events"][0], event_type, client_id, request_id,
                             session_id if kind == "list_commands" else None)
        elif not ready:
            raise RuntimeError(f"query was rejected: {reply['outcome']!r}")
        if phase == "sample" and evidence is not None:
            evidence.sample("uds_direct_read", elapsed_us=math.ceil(elapsed * 1000))
        return elapsed, reply["outcome"]

    # Composition readiness is outside measurement and never waits for a read on SSE.
    deadline = time.monotonic() + 5
    attempt = 0
    while True:
        _, outcome = query(attempt, "ready", ready=True)
        if outcome["type"] == "accepted":
            break
        if outcome["error"].get("code") != "session_not_loaded" or time.monotonic() >= deadline:
            raise RuntimeError(f"session did not become query-ready: {outcome!r}")
        attempt += 1
        time.sleep(0.001)

    def control(index: int, phase: str) -> float:
        request_id = f"m4-sse-{phase}-{index}"
        encoded = payload("resume_session", request_id)
        started = time.perf_counter_ns()
        commands.send_request("POST", "/v1/command", headers, encoded)
        # Resume an already loaded session as observer. Its connection-scoped result
        # exercises the actual host event lane, independently of direct read bodies.
        event = events.next_matching_event(request_id, "sessions_listed")
        elapsed = (time.perf_counter_ns() - started) / 1_000_000
        correlated_event(event, "sessions_listed", client_id, request_id)
        sessions = event.get("sessions")
        if not isinstance(sessions, list) or not any(
            isinstance(session, dict) and session.get("session_id") == session_id
            for session in sessions
        ):
            raise RuntimeError("observer control result omitted its session")
        reply = command_reply(read_reply(), "command")
        if reply["outcome"]["type"] != "accepted":
            raise RuntimeError(f"observer resume was rejected: {reply['outcome']!r}")
        if phase == "sample" and evidence is not None:
            evidence.sample("uds_control_event", elapsed_us=math.ceil(elapsed * 1000))
        return elapsed

    if evidence is not None:
        evidence.update(socket_event_command="resume_session", socket_event_type="sessions_listed",
                        socket_read_commands=["list_commands", "list_models"])

    for index in range(min(50, max(10, samples // 10))):
        query(index, "warmup")
        control(index, "warmup")
    read_latencies: list[float] = []
    event_latencies: list[float] = []
    for index in range(samples):
        read_latencies.append(query(index, "sample")[0])
        event_latencies.append(control(index, "sample"))

    def p99(values: list[float]) -> float:
        return sorted(values)[max(0, math.ceil(len(values) * 0.99) - 1)]

    event_p99 = p99(event_latencies)
    read_p99 = p99(read_latencies)
    for channel, values in [("control SSE event", event_latencies), ("direct read reply", read_latencies)]:
        print(f"M4 production UDS {channel}: samples={samples}; "
              f"p50={statistics.median(values):.3f}ms p99={p99(values):.3f}ms "
              f"max={max(values):.3f}ms")
    if evidence is not None:
        evidence.update(uds_direct_read_p99_us=math.ceil(read_p99 * 1000))
    if event_p99 >= 2:
        raise RuntimeError(f"production engine-to-TUI socket event p99 {event_p99:.3f}ms exceeds 2ms")
    return {"uds_event_p99_us": math.ceil(event_p99 * 1000)}
