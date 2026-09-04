#!/usr/bin/env python3
"""Run one CI gate, preserve its exit status and bounded failure evidence."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import selectors
import subprocess
import sys
import time

MAX_TAIL_BYTES = 128 * 1024


def group_has_live_members(group: int) -> bool:
    output = subprocess.check_output(
        ["ps", "-axo", "pgid=,stat="], stderr=subprocess.DEVNULL, timeout=2,
    ).decode()
    rows = [line.split() for line in output.splitlines() if line.strip()]
    if not rows or any(len(row) != 2 or not row[0].isdigit() for row in rows):
        raise OSError("process group status unavailable")
    return any(int(pgid) == group and not status.startswith("Z") for pgid, status in rows)


def settle_group(process: subprocess.Popen) -> None:
    # Darwin can return EPERM for an orphaned zombie-only group. That is not
    # proof of a live process, nor is every EPERM safe to ignore.
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            return
        except PermissionError:
            if not group_has_live_members(process.pid):
                return
            raise
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            process.poll()
            try:
                os.killpg(process.pid, 0)
            except ProcessLookupError:
                return
            except PermissionError:
                if not group_has_live_members(process.pid):
                    return
                raise
            if not group_has_live_members(process.pid):
                return
            time.sleep(0.02)
    raise OSError("process group cleanup deadline exceeded")


def write_result(path: Path, result: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(result, sort_keys=True) + "\n")
    temporary.replace(path)


def observe(command: list[str], gate: str, output: Path) -> int:
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

    process = None
    exit_code = 1
    previous = signal.getsignal(signal.SIGTERM)

    def interrupted(_signal, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    try:
        process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True)
        assert process.stdout is not None
        last_checkpoint = started
        leader_exited_at = None
        with selectors.DefaultSelector() as ready:
            ready.register(process.stdout, selectors.EVENT_READ)
            while True:
                now = time.monotonic()
                if process.poll() is not None:
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
                    tail.extend(chunk)
                    del tail[:-MAX_TAIL_BYTES]
                if now - last_checkpoint >= 5:
                    result.update(elapsed_seconds=now - started, log_tail=tail.decode(errors="replace"))
                    write_result(output, result)
                    last_checkpoint = now
        final = redact(b"", final=True)
        sys.stdout.buffer.write(final)
        sys.stdout.buffer.flush()
        tail.extend(final)
        del tail[:-MAX_TAIL_BYTES]
        exit_code = process.wait()
    except KeyboardInterrupt:
        exit_code = 130
    except OSError as error:
        result["launch_error"] = str(error)
    finally:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        if process is not None:
            # Descendants may retain pipes after the direct child exits. The
            # owned group must be settled even when its leader is already gone.
            try:
                settle_group(process)
                process.wait(timeout=2)
            except (OSError, subprocess.SubprocessError) as error:
                result["cleanup_error"] = type(error).__name__
                if exit_code == 0:
                    exit_code = 1
            if process.stdout is not None:
                process.stdout.close()
        if exit_code < 0:
            exit_code = 128 - exit_code
        result.update(status="passed" if exit_code == 0 else "failed", exit_code=exit_code,
                      elapsed_seconds=time.monotonic() - started, log_tail=tail.decode(errors="replace"))
        write_result(output, result)
        signal.signal(signal.SIGTERM, previous)
    return exit_code


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gate", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("a command is required")
    return observe(command, args.gate, args.output)


if __name__ == "__main__":
    raise SystemExit(main())
