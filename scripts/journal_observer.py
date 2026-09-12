"""Observe the public segmented journal format in acceptance fixtures.

This is a harness reader, not an engine committed-prefix capability. Crash/restart
acceptance establishes persistence; merely seeing an active record does not.
"""
from __future__ import annotations

import json
import re
from pathlib import Path

SEALED_NAME = re.compile(r"[0-9]{20}-[0-9]{20}-[0-9]{20}-[0-9a-f]{64}\.jsonl")


def session_journals(sessions_root: Path) -> list[Path]:
    if list(sessions_root.glob("*/events.jsonl")):
        raise RuntimeError("unsupported lifetime-file journal in acceptance storage")
    return sorted(path for path in sessions_root.glob("*/journal") if path.is_dir())


def journal_files(journal: Path) -> list[Path]:
    sealed = []
    for path in journal.glob("*.jsonl"):
        if path.name == "active.jsonl":
            continue
        if not SEALED_NAME.fullmatch(path.name):
            raise RuntimeError(f"invalid journal segment name: {path.name}")
        sealed.append(path)
    active = journal / "active.jsonl"
    return sorted(sealed) + ([active] if active.is_file() else [])


def observed_envelopes(journal: Path) -> list[dict[str, object]]:
    """Read a short acceptance transcript, retrying a concurrent segment seal."""
    for _ in range(4):
        before = journal.stat().st_mtime_ns
        events = []
        try:
            for path in journal_files(journal):
                raw = path.read_bytes()
                records = raw.splitlines(keepends=True)
                for record in records:
                    if not record.endswith(b"\n") and path.name == "active.jsonl":
                        continue
                    envelope = json.loads(record)
                    if not isinstance(envelope, dict):
                        raise RuntimeError("journal envelope is not an object")
                    events.append(envelope)
        except FileNotFoundError:
            continue
        if journal.stat().st_mtime_ns == before:
            return events
    raise RuntimeError("journal kept rotating during acceptance observation")
