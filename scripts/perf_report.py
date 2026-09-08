"""Bounded decoding of private native measurement reports."""
from __future__ import annotations

import json
import os
from pathlib import Path
import stat

INPUT_REPORT_BYTES = 256 * 1024
MEMORY_REPORT_BYTES = 8 * 1024 * 1024


def read_report(path: Path, limit: int) -> dict:
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(descriptor, 'rb') as source:
        metadata = os.fstat(source.fileno())
        if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.geteuid()
                or metadata.st_nlink != 1 or metadata.st_size > limit):
            raise ValueError('measurement report is not an owned bounded file')
        encoded = source.read(limit + 1)
        if len(encoded) > limit:
            raise ValueError('measurement report exceeded its byte allowance')
    result = json.loads(encoded)
    if not isinstance(result, dict):
        raise ValueError('measurement report must be an object')
    return result
