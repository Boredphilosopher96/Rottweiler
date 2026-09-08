"""Bounded, turn-qualified observations of the soak's canonical journal."""
from __future__ import annotations

from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re

from journal_observer import journal_files, session_journals

READ_CHUNK = 64 * 1024
MAX_RECORD = 1024 * 1024
MAX_POLL_BYTES = 4 * 1024 * 1024
MAX_FILES = 4096
IDENTITY = re.compile(r"[A-Za-z0-9_.:-]{1,160}")
DECIMAL = re.compile(r"(?:0|[1-9][0-9]{0,19})")


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate journal object key")
        result[key] = value
    return result


@dataclass(frozen=True)
class RecordProof:
    file: tuple[int, int]
    offset: int
    size: int
    digest: bytes


@dataclass
class PendingStep:
    marker: str
    kind: str
    accepted: bool = False
    turn: str | None = None
    started: bool = False
    deltas: int = 0
    invocation: str | None = None
    tool_source: str | None = None
    tool_committed: bool = False
    body: RecordProof | None = None
    compact_cause: str | None = None
    completed: bool = False

    @property
    def input_marker(self):
        return self.marker.replace("SOAK_STEP_", "SOAK_INPUT_").removesuffix("_DONE")


class EventLogProbe:
    """Read bounded growth and retain only current work plus the last proof.

    Active journal bytes are observations, not a committed-prefix capability.
    The forced TUI restart separately rereads the exact terminal/body records;
    a process crash/durability gate remains responsible for engine crash repair.
    """
    def __init__(self, sessions_root: Path):
        self.sessions_root = sessions_root
        self.paths: dict[tuple[int, int], Path] = {}
        self.offsets: dict[tuple[int, int], int] = {}
        self.pending_records: dict[tuple[int, int], bytearray] = {}
        self.last_events: dict[tuple[int, int], dict] = {}
        self.event_counts: dict[str, int] = {}
        self.bytes_observed = 0
        self.session: str | None = None
        self.sequence = -1
        self.pending: PendingStep | None = None
        self.last_marker: str | None = None
        self.last_proof: tuple[RecordProof, RecordProof] | None = None

    def begin(self, marker: str, kind: str):
        if not re.fullmatch(r"SOAK_STEP_[0-9]{6}_DONE", marker) or kind not in ("turn", "tool", "compact"):
            raise ValueError("invalid soak step")
        if self.pending is not None and not self.pending.completed:
            raise RuntimeError("cannot replace an unfinished soak step")
        self.pending = PendingStep(marker, kind)

    def poll(self, marker: str | None = None) -> bool:
        remaining = MAX_POLL_BYTES
        for journal in session_journals(self.sessions_root):
            for path in journal_files(journal):
                if remaining == 0:
                    return self._completed(marker)
                try:
                    with path.open("rb") as handle:
                        metadata = os.fstat(handle.fileno())
                        identity = metadata.st_dev, metadata.st_ino
                        if identity not in self.paths and len(self.paths) == MAX_FILES:
                            raise RuntimeError("soak journal file inventory exceeds bound")
                        self.paths[identity] = path
                        offset = self.offsets.get(identity, 0)
                        if metadata.st_size < offset:
                            raise RuntimeError("observed soak journal prefix was truncated")
                        handle.seek(offset)
                        pending = self.pending_records.setdefault(identity, bytearray())
                        while remaining:
                            raw = handle.read(min(READ_CHUNK, remaining))
                            if not raw:
                                if pending and path.name != "active.jsonl":
                                    raise ValueError("sealed soak segment ends in a partial record")
                                break
                            self.bytes_observed += len(raw)
                            remaining -= len(raw)
                            start = offset - len(pending)
                            offset += len(raw)
                            pending.extend(raw)
                            while True:
                                newline = pending.find(b"\n")
                                if newline < 0:
                                    break
                                if newline + 1 > MAX_RECORD:
                                    raise RuntimeError("soak journal record exceeds bound")
                                record = bytes(pending[:newline + 1])
                                del pending[:newline + 1]
                                proof = RecordProof(identity, start, len(record), hashlib.sha256(record).digest())
                                self._observe(record, path, proof)
                                start += len(record)
                            if len(pending) > MAX_RECORD:
                                raise RuntimeError("unterminated soak journal record exceeds bound")
                            self.offsets[identity] = offset
                except FileNotFoundError:
                    # A segment may be renamed between enumeration and open.
                    # Its inode/prefix is consumed on the next bounded poll.
                    continue
        return self._completed(marker)

    def _completed(self, marker):
        return bool(marker is not None and self.pending is not None
                    and self.pending.marker == marker and self.pending.completed)

    def _observe(self, record: bytes, path: Path, proof: RecordProof):
        envelope = json.loads(record, object_pairs_hook=unique_object)
        if not isinstance(envelope, dict) or not isinstance(envelope.get("event"), dict):
            raise ValueError("invalid canonical soak event")
        event = envelope["event"]
        meta = event.get("meta")
        if not isinstance(meta, dict):
            raise ValueError("soak event lacks canonical identity")
        session, sequence = meta.get("session_id"), meta.get("sequence_id")
        if not isinstance(session, str) or not IDENTITY.fullmatch(session) or session != path.parent.parent.name:
            raise ValueError("soak event session identity mismatch")
        if not isinstance(sequence, str) or not DECIMAL.fullmatch(sequence):
            raise ValueError("invalid soak event sequence")
        if self.session is None:
            self.session = session
        if self.session != session or int(sequence) != self.sequence + 1:
            raise ValueError("soak event prefix is not one contiguous session")
        self.sequence = int(sequence)
        kind = event.get("type")
        if not isinstance(kind, str) or not re.fullmatch(r"[a-z_]{1,100}", kind):
            raise ValueError("invalid soak event discriminator")
        if kind not in self.event_counts and len(self.event_counts) == 256:
            raise ValueError("soak event discriminator inventory exceeds bound")
        self.event_counts[kind] = self.event_counts.get(kind, 0) + 1
        fields = {"session_id": session, "sequence_id": sequence,
                  "turn_id": event.get("turn_id"), "request_id": meta.get("caused_by"), "event_type": kind}
        self.last_events[proof.file] = {
            key: value if isinstance(value, str) and IDENTITY.fullmatch(value) else None
            for key, value in fields.items()
        }
        pending = self.pending
        if pending is None or pending.completed:
            return
        if kind == "user_message_accepted" and pending.kind != "compact":
            text = event.get("content")
            if isinstance(text, str) and pending.input_marker in text:
                if pending.accepted:
                    raise RuntimeError("soak input was accepted more than once")
                turn = self._turn(event.get("agent_turn"))
                if not pending.started or pending.turn != turn:
                    raise RuntimeError("soak acceptance lacks its exact started turn")
                pending.accepted = True
        elif kind == "compaction_started" and pending.kind == "compact" and event.get("reason") == "manual":
            if pending.accepted:
                raise RuntimeError("multiple manual compactions for one soak step")
            pending.accepted = True
            pending.compact_cause = meta.get("caused_by")
        elif kind == "turn_started" and pending.kind != "compact":
            if pending.started:
                raise RuntimeError("multiple active turns for one soak step")
            pending.turn = self._turn(event.get("turn_id"))
            pending.started = True
        elif kind == "text_delta" and pending.started and event.get("turn_id") == pending.turn:
            if isinstance(event.get("text"), str) and event["text"]:
                pending.deltas += 1
        elif kind == "tool_call_started" and pending.kind == "tool" and pending.started and event.get("turn_id") == pending.turn:
            if event.get("name") == "read" and event.get("args") == {"path": "soak.txt"}:
                pending.invocation = event.get("invocation_id")
        elif kind == "tool_call_finished" and pending.invocation is not None and event.get("invocation_id") == pending.invocation and event.get("turn_id") == pending.turn:
            if event.get("is_error") is not False:
                raise RuntimeError("soak read tool failed")
            pending.tool_source = sequence
        elif kind == "conversation_tool_results_committed" and event.get("agent_turn") == pending.turn and pending.tool_source is not None:
            expected = {"invocation_id": pending.invocation, "finished_source": pending.tool_source}
            pending.tool_committed |= expected in event.get("results", [])
        elif kind == "conversation_turn_committed" and pending.accepted:
            turn = event.get("turn")
            if not isinstance(turn, dict) or turn.get("role") != "assistant":
                return
            blocks = turn.get("blocks", [])
            text = "".join(block.get("text", "") for block in blocks if isinstance(block, dict) and block.get("type") == "text")
            if pending.marker not in text:
                return
            agent_turn = self._turn(event.get("agent_turn"))
            if pending.kind == "compact":
                if turn.get("meta", {}).get("summary") is not True:
                    return
                pending.turn = agent_turn
            elif agent_turn != pending.turn:
                return
            pending.body = proof
        elif kind == "turn_finished" and pending.kind != "compact" and event.get("turn_id") == pending.turn:
            if event.get("status") != "completed":
                raise RuntimeError("soak turn did not complete successfully")
            if not pending.started or pending.body is None or pending.deltas == 0:
                raise RuntimeError("soak turn finished without streamed committed fixture response")
            if pending.kind == "tool" and not pending.tool_committed:
                raise RuntimeError("soak tool turn lacks an exact successful committed read result")
            self._finish(pending, proof)
        elif kind == "compaction_finished" and pending.kind == "compact" and pending.accepted:
            if pending.body is None or event.get("summary_turn_id") != pending.turn or meta.get("caused_by") != pending.compact_cause:
                raise RuntimeError("soak compaction finished without its exact committed summary")
            self._finish(pending, proof)
        elif kind == "compaction_failed" and pending.kind == "compact" and pending.accepted:
            raise RuntimeError("soak manual compaction failed")

    @staticmethod
    def _turn(value):
        if not isinstance(value, str) or not DECIMAL.fullmatch(value):
            raise ValueError("invalid soak turn identity")
        return value

    def _finish(self, pending, proof):
        pending.completed = True
        self.last_marker = pending.marker
        self.last_proof = pending.body, proof

    def saw(self, marker):
        return self.pending is not None and marker == self.pending.input_marker and self.pending.accepted

    def event_count(self, kind):
        return self.event_counts.get(kind, 0)

    def marker_persisted(self, marker):
        if marker != self.last_marker or self.last_proof is None:
            return False
        self.poll()
        for proof in self.last_proof:
            try:
                with self.paths[proof.file].open("rb") as handle:
                    meta = os.fstat(handle.fileno())
                    if (meta.st_dev, meta.st_ino) != proof.file:
                        return False
                    handle.seek(proof.offset)
                    if hashlib.sha256(handle.read(proof.size)).digest() != proof.digest:
                        return False
            except FileNotFoundError:
                return False
        return True

    def diagnostics(self):
        return [{**self.last_events.get(file, {}), "observed_bytes": offset}
                for file, offset in sorted(self.offsets.items())[-16:]]

    def durable_bytes(self):
        return sum(path.stat().st_size for journal in session_journals(self.sessions_root)
                   for path in journal_files(journal))
