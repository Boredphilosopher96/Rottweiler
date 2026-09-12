#!/usr/bin/env python3
"""Run the source-owned TUI test VMs with explicit physical settlement."""
from __future__ import annotations

import argparse
import os
from pathlib import Path
import subprocess
import time

from perf_process_owner import OwnedProcess, SCOPE

ROOT = Path(__file__).resolve().parents[1]


def commands(mode: str) -> list[list[str]]:
    if mode == "test":
        return [["bun", "test", "--max-concurrency=1", "--path-ignore-patterns=test/perf/**"]]
    if mode == "test:perf":
        return [["bun", "test", "test/perf/m4-transport-performance.test.ts"],
                ["bun", "test", "test/perf/performance.test.ts"]]
    raise ValueError("unknown TUI verification mode")


def run(mode: str) -> None:
    for command in commands(mode):
        SCOPE.check()
        owner = OwnedProcess(command, cwd=ROOT / "packages/tui", env=dict(os.environ),
                             delegated=True, output="inherit")
        try:
            while owner.observe_exit() is None:
                SCOPE.check()
                owner.scope.drain()
                time.sleep(.02)
            code = owner.observe_exit()
        finally:
            owner.settle()
        if code:
            raise subprocess.CalledProcessError(code, command)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["test", "test:perf"])
    args = parser.parse_args()
    try:
        run(args.mode)
    except subprocess.CalledProcessError as error:
        return 128 - error.returncode if error.returncode < 0 else error.returncode
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
