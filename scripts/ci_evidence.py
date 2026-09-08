#!/usr/bin/env python3
"""Run one CI gate, preserve its exit status and bounded failure evidence."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import selectors
import subprocess
import sys
import time

from perf_process_owner import OwnedProcess, SCOPE
from perf_process_scope import ScopeCancelled, UnsettledScope

MAX_TAIL_BYTES = 128 * 1024
MAX_LOG_BYTES = 8 * 1024 * 1024


def write_result(path: Path, result: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(result, sort_keys=True) + "\n")
    temporary.replace(path)


def observe(command: list[str], gate: str, output: Path, *, delegated: bool = False) -> int:
    started = time.monotonic()
    checkout = subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True, text=True, check=False)
    result = {
        "schema_version": 1, "gate": gate, "status": "running",
        "source_sha": checkout.stdout.strip() if checkout.returncode == 0 else None,
        "workflow_sha": os.environ.get("GITHUB_SHA"),
        "run_id": os.environ.get("GITHUB_RUN_ID"),
        "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
        "runner_os": os.environ.get("RUNNER_OS", sys.platform),
        "runner_arch": os.environ.get("RUNNER_ARCH"),
        "image_version": os.environ.get("ImageVersion"),
        "started_at_unix": time.time(),
        "lockfiles": {
            str(path): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in [Path("Cargo.lock"), *Path("packages").glob("*/bun.lock")]
            if path.is_file()
        },
    }
    write_result(output, result)
    log_path = output.with_suffix(".log")
    log = log_path.open("wb", buffering=0)
    result.update(log_file=log_path.name, log_prefix_bytes=0, log_omitted_bytes=0)
    secret_values = [value.encode() for key, value in os.environ.items()
                     if any(word in key.upper() for word in ("TOKEN", "SECRET", "PASSWORD", "API_KEY")) and len(value) >= 6]
    tail = bytearray()
    pending = bytearray()
    held_bytes = max((len(value) for value in secret_values), default=1) - 1

    def redact(chunk: bytes, final: bool = False) -> bytes:
        pending.extend(chunk)
        for secret in secret_values:
            pending[:] = pending.replace(secret, b"[REDACTED]")
        emit = len(pending) if final else max(0, len(pending) - held_bytes)
        safe = bytes(pending[:emit])
        del pending[:emit]
        return safe

    def retain(chunk: bytes) -> None:
        # Keep early failures even when later test binaries fill the rolling tail.
        # Both artifacts receive only the stream after cross-read secret redaction.
        available = MAX_LOG_BYTES - result["log_prefix_bytes"]
        prefix = chunk[:available]
        log.write(prefix)
        result["log_prefix_bytes"] += len(prefix)
        result["log_omitted_bytes"] += len(chunk) - len(prefix)
        tail.extend(chunk)
        del tail[:-MAX_TAIL_BYTES]

    owner = None
    exit_code = 1
    try:
        owner = OwnedProcess(command, cwd=Path.cwd(), env=dict(os.environ),
                             delegated=delegated, combined_output=True)
        process = owner.process
        assert process.stdout is not None
        last_checkpoint = started
        leader_exited_at = None
        with selectors.DefaultSelector() as ready:
            ready.register(process.stdout, selectors.EVENT_READ)
            while True:
                now = time.monotonic()
                SCOPE.check()
                if owner.scope is not None:
                    owner.scope.drain()
                if owner.observe_exit() is not None:
                    if leader_exited_at is None:
                        leader_exited_at = now
                    elif now - leader_exited_at >= 0.5:
                        break
                if ready.select(timeout=0.1):
                    chunk = os.read(process.stdout.fileno(), 16 * 1024)
                    if not chunk:
                        break
                    chunk = redact(chunk)
                    sys.stdout.buffer.write(chunk)
                    sys.stdout.buffer.flush()
                    retain(chunk)
                if now - last_checkpoint >= 5:
                    result.update(elapsed_seconds=now - started, log_tail=tail.decode(errors="replace"))
                    write_result(output, result)
                    last_checkpoint = now
        final = redact(b"", final=True)
        sys.stdout.buffer.write(final)
        sys.stdout.buffer.flush()
        retain(final)
        while (status := owner.observe_exit()) is None:
            SCOPE.check()
            if owner.scope is not None:
                owner.scope.drain()
            time.sleep(.01)
        exit_code = status
    except (KeyboardInterrupt, ScopeCancelled):
        exit_code = 130
    except (OSError, UnsettledScope) as error:
        result["launch_error"] = str(error)
    finally:
        if owner is not None:
            try:
                owner.settle()
            except (OSError, subprocess.SubprocessError, UnsettledScope) as error:
                result["cleanup_error"] = str(error)
                if exit_code == 0:
                    exit_code = 1
        if exit_code < 0:
            exit_code = 128 - exit_code
        result.update(status="passed" if exit_code == 0 else "failed", exit_code=exit_code,
                      elapsed_seconds=time.monotonic() - started, log_tail=tail.decode(errors="replace"))
        log.close()
        write_result(output, result)
    return exit_code


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gate", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--delegated", action="store_true", help="require nested process-owner settlement acknowledgement")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("a command is required")
    return observe(command, args.gate, args.output, delegated=args.delegated)


if __name__ == "__main__":
    raise SystemExit(main())
