#!/usr/bin/env python3
"""Build the checksum-verified native renderer under this worktree's target."""
import contextlib
import os
from pathlib import Path
import sys

import opentui_native

ROOT = Path(__file__).resolve().parents[1]
if __name__ == "__main__":
    target = Path(os.environ.get("CARGO_TARGET_DIR", str(ROOT / "target"))).resolve()
    with contextlib.redirect_stdout(sys.stderr):
        library = opentui_native.build(ROOT, target)
    print(library)
