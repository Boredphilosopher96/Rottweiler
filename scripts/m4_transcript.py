"""Independent oracle for the input and answer in the M4 loopback fixture."""
from __future__ import annotations


def fixture_turns(envelopes: list[dict]) -> list[dict]:
    accepted: dict[str, dict] = {}
    claimed: set[str] = set()
    turns = []
    for envelope in envelopes:
        event = envelope.get("event")
        if not isinstance(event, dict):
            raise RuntimeError("fixture journal event is not an object")
        kind = event.get("type")
        if kind == "user_message_accepted":
            sequence = envelope.get("sequence")
            if not isinstance(sequence, str) or not sequence.isdecimal() or sequence in accepted:
                raise RuntimeError("fixture accepted input source is invalid")
            if event.get("meta", {}).get("sequence_id") != sequence:
                raise RuntimeError("fixture input envelope identity differs")
            accepted[sequence] = event
        elif kind == "conversation_input_committed":
            source = event.get("accepted_source")
            if not isinstance(source, str) or source not in accepted or source in claimed:
                raise RuntimeError("fixture input has a missing or reused source")
            original = accepted[source]
            if (original.get("attachments") != [] or event.get("selection") != {"type": "accepted"}
                    or original.get("meta", {}).get("session_id") != event.get("meta", {}).get("session_id")
                    or original.get("agent_turn") != event.get("agent_turn")):
                raise RuntimeError("fixture input selection differs from the declared workload")
            text = original.get("content")
            if not isinstance(text, str):
                raise RuntimeError("fixture input has no text")
            claimed.add(source)
            turns.append({"role": "user", "blocks": [{"type": "text", "text": text}]})
        elif kind == "conversation_turn_committed":
            turn = event.get("turn")
            if not isinstance(turn, dict) or turn.get("role") != "assistant" or not isinstance(turn.get("blocks"), list):
                raise RuntimeError("fixture conversation requires a source input and assistant answer")
            turns.append({"role": "assistant", "blocks": turn["blocks"]})
    return turns
