#!/usr/bin/env python3
"""Verify a prepared source renderer without compiling or downloading anything."""
import json
import os
from pathlib import Path
import platform
import sys

import opentui_native
from release_contract import load_contract

ROOT = Path(__file__).resolve().parents[1]


def verified_library(value: str) -> Path:
    library = Path(value)
    if not library.is_absolute():
        raise ValueError("ROTTWEILER_OPENTUI_LIBRARY must be an absolute prepared library path")
    receipt = library.parent / opentui_native.RECEIPT
    if receipt.is_symlink() or receipt.stat().st_size > 128 * 1024:
        raise ValueError("native renderer receipt must be bounded regular data")
    observed = json.loads(receipt.read_text())["identity"]
    host = load_contract(ROOT / "contracts/release-contract.json").resolve_platform(platform.system(), platform.machine())
    opentui_native.validate_identity(ROOT, host.id, observed)
    result = opentui_native.verify(library.parent, observed)
    if result != library:
        raise ValueError("selected library is not the receipt's native artifact")
    return result


if __name__ == "__main__":
    try:
        print(verified_library(os.environ.get("ROTTWEILER_OPENTUI_LIBRARY", "")))
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"Source renderer unavailable: {error}\nPrepare and export ROTTWEILER_OPENTUI_LIBRARY with python3 scripts/build-opentui-native.py before running source TUI commands.", file=sys.stderr)
        sys.exit(1)
