#!/usr/bin/env python3
"""Fail closed until 14 consecutive, zero-P0 self-hosting days are evidenced."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import re
import stat
from pathlib import Path


MAX_LEDGER_BYTES = 1024 * 1024
MAX_RECORDS = 366
REQUIRED_DAYS = 14
COMMIT = re.compile(r"[0-9a-f]{7,40}")
KEYS = {"date", "commit", "session_ids", "p0_incidents"}


def read_ledger(path: Path) -> list[dict[str, object]]:
    metadata = path.lstat()
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        raise ValueError("dogfood ledger must be a regular non-symlink file")
    if metadata.st_size > MAX_LEDGER_BYTES:
        raise ValueError("dogfood ledger exceeds 1 MiB")
    with path.open("rb") as handle:
        data = handle.read(MAX_LEDGER_BYTES + 1)
        after = path.stat()
    if len(data) > MAX_LEDGER_BYTES:
        raise ValueError("dogfood ledger grew beyond 1 MiB")
    if (metadata.st_dev, metadata.st_ino, metadata.st_size) != (
        after.st_dev,
        after.st_ino,
        after.st_size,
    ):
        raise ValueError("dogfood ledger changed while it was read")
    lines = data.decode("utf-8").splitlines()
    if len(lines) > MAX_RECORDS:
        raise ValueError("dogfood ledger exceeds 366 records")
    records: list[dict[str, object]] = []
    for number, line in enumerate(lines, 1):
        if not line.strip():
            raise ValueError(f"blank ledger record at line {number}")
        value = json.loads(line)
        if not isinstance(value, dict) or set(value) != KEYS:
            raise ValueError(f"invalid ledger schema at line {number}")
        records.append(value)
    return records


def check_gate(records: list[dict[str, object]], through: dt.date) -> dict[str, object]:
    dates: list[dt.date] = []
    for number, record in enumerate(records, 1):
        try:
            date = dt.date.fromisoformat(str(record["date"]))
        except ValueError as error:
            raise ValueError(f"invalid UTC date at line {number}") from error
        commit = record["commit"]
        sessions = record["session_ids"]
        p0_incidents = record["p0_incidents"]
        if not isinstance(commit, str) or COMMIT.fullmatch(commit) is None:
            raise ValueError(f"invalid commit at line {number}")
        if (
            not isinstance(sessions, list)
            or not 1 <= len(sessions) <= 64
            or any(not isinstance(item, str) or not 1 <= len(item) <= 128 for item in sessions)
            or len(sessions) != len(set(sessions))
        ):
            raise ValueError(f"invalid session evidence at line {number}")
        if not isinstance(p0_incidents, int) or isinstance(p0_incidents, bool) or p0_incidents < 0:
            raise ValueError(f"invalid P0 count at line {number}")
        if p0_incidents != 0:
            raise ValueError(f"P0 incident recorded on {date.isoformat()}")
        dates.append(date)
    if dates != sorted(set(dates)):
        raise ValueError("ledger dates must be unique and ascending")
    window = dates[-REQUIRED_DAYS:]
    expected = [through - dt.timedelta(days=offset) for offset in reversed(range(REQUIRED_DAYS))]
    if window != expected:
        raise ValueError(
            f"need {REQUIRED_DAYS} consecutive zero-P0 days ending {through.isoformat()}"
        )
    return {
        "gate": "self_hosting",
        "status": "pass",
        "consecutive_days": REQUIRED_DAYS,
        "through": through.isoformat(),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("ledger", type=Path)
    parser.add_argument(
        "--through",
        type=dt.date.fromisoformat,
        default=dt.datetime.now(dt.UTC).date(),
    )
    args = parser.parse_args()
    print(json.dumps(check_gate(read_ledger(args.ledger), args.through), sort_keys=True))


if __name__ == "__main__":
    main()
