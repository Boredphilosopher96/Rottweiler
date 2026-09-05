#!/usr/bin/env python3
"""Build the native sandbox test prerequisite and report its Cargo-owned executable."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
BINARY = "rw-sandbox-helper"
ENVIRONMENT_KEY = "ROTTWEILER_TEST_SANDBOX_HELPER"


def build() -> Path:
    command = ["cargo", "build", "--locked", "--all-features", "-p", "rw-sandbox",
               "--bin", BINARY, "--message-format=json-render-diagnostics"]
    executable = None
    # Inherit this worktree's target/profile and stream Cargo output. The helper
    # is a test prerequisite; acceptance measurements never compile it implicitly.
    with subprocess.Popen(command, cwd=ROOT, stdout=subprocess.PIPE, text=True) as process:
        assert process.stdout is not None
        for line in process.stdout:
            message = json.loads(line)
            if (message.get("reason") == "compiler-artifact"
                    and message.get("target", {}).get("name") == BINARY
                    and "bin" in message.get("target", {}).get("kind", [])
                    and message.get("executable")):
                executable = Path(message["executable"])
        if process.wait() != 0:
            raise RuntimeError("sandbox test helper build failed")
    if executable is None or not executable.is_file() or not os.access(executable, os.X_OK):
        raise RuntimeError("Cargo did not produce an executable sandbox test helper")
    return executable.resolve(strict=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--github-env", type=Path)
    args = parser.parse_args()
    executable = build()
    if args.github_env is not None:
        if any(character in str(executable) for character in "\r\n"):
            raise ValueError("helper executable path cannot contain a line break")
        with args.github_env.open("a") as stream:
            stream.write(f"{ENVIRONMENT_KEY}={executable}\n")
    print(executable)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError) as error:
        print(f"sandbox test prerequisite: {error}", file=sys.stderr)
        raise SystemExit(1) from error
