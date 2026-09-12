#!/usr/bin/env python3
"""Own one M8 daemon container through explicit removal and absence proof."""
from __future__ import annotations

import argparse
import os
from pathlib import Path
import re
import selectors
import subprocess
import sys
import time

from perf_process_deadline import RESOURCE_CLEANUP_SECONDS
from perf_process_owner import OwnedProcess, SCOPE
from perf_process_scope import ScopeCancelled, UnsettledScope

CONTROL_BYTES = 16 * 1024
CLEANUP_SECONDS = 2
CANCEL_CREATION_SECONDS = 4


def retirement_deadline() -> float:
    return (SCOPE.cancelled_at if SCOPE.cancelled_at is not None else time.monotonic()) + RESOURCE_CLEANUP_SECONDS


def control(command: list[str], parent: str, *, cleanup: bool = False, deadline: float | None = None) -> bytes:
    """A creation reply must finish or remain ambiguous; cancellation cannot fake it."""
    if cleanup and deadline is None:
        raise ValueError("daemon cleanup requires its shared retirement deadline")
    if deadline is not None and time.monotonic() >= deadline:
        raise UnsettledScope("UNSETTLED Docker cleanup deadline expired before launch")
    owner = OwnedProcess(command, cwd=Path.cwd(), env=dict(os.environ),
                         output="stdout", cleanup_of=parent)
    output = bytearray()
    stage_deadline = min(deadline, time.monotonic() + CLEANUP_SECONDS) if cleanup else None
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(owner.process.stdout, selectors.EVENT_READ)
            while selector.get_map() or owner.observe_exit() is None:
                if not cleanup and SCOPE.cancelled and stage_deadline is None:
                    deadline = retirement_deadline()
                    stage_deadline = min(deadline, SCOPE.cancelled_at + CANCEL_CREATION_SECONDS)
                if stage_deadline is not None and time.monotonic() >= stage_deadline:
                    raise UnsettledScope("UNSETTLED Docker control reply; daemon operation may still be pending")
                for key, _ in selector.select(.02):
                    chunk = os.read(key.fd, min(4096, CONTROL_BYTES + 1 - len(output)))
                    if not chunk:
                        selector.unregister(key.fileobj)
                    else:
                        output.extend(chunk)
                        if len(output) > CONTROL_BYTES:
                            raise UnsettledScope("UNSETTLED Docker control output exceeded admission")
        if stage_deadline is not None and time.monotonic() >= stage_deadline:
            raise UnsettledScope("UNSETTLED Docker control reply exceeded retirement deadline")
        status = owner.observe_exit()
        if status != 0:
            raise RuntimeError(f"Docker control exited {status}: {output.decode(errors='replace')}")
        return bytes(output)
    finally:
        owner.settle(deadline=deadline if deadline is not None else (retirement_deadline() if SCOPE.cancelled else None))


def run(name: str, command: list[str]) -> int:
    if not re.fullmatch(r"rottweiler-m8-[0-9]+-[0-9]+", name):
        raise ValueError("invalid M8 container identity")
    if command[:2] != ["docker", "run"] or command.count("--name") != 1:
        raise ValueError("M8 requires one explicit Docker container")
    if command[command.index("--name") + 1] != name or "--rm" not in command:
        raise ValueError("M8 container identity and removal policy must match")
    parent = SCOPE.starting()
    container = None
    attached = None
    failure = None
    creation_started = False
    try:
        SCOPE.check()
        create = ["docker", "create", *[value for value in command[2:] if value != "--rm"]]
        creation_started = True
        container = control(create, parent).decode().strip()
        if not re.fullmatch(r"[0-9a-f]{64}", container):
            raise UnsettledScope(f"UNSETTLED Docker creation identity for {name}")
        SCOPE.check()
        attached = OwnedProcess(["docker", "start", "--attach", container],
                                cwd=Path.cwd(), env=dict(os.environ), output="inherit")
        while attached.observe_exit() is None:
            SCOPE.check()
            time.sleep(.02)
        status = attached.observe_exit()
        if status != 0:
            return 128 - status if status < 0 else status
        state = control(["docker", "inspect", "--format", "{{.State.Status}} {{.State.ExitCode}}", container], parent)
        match = re.fullmatch(rb"exited ([0-9]+)\s*", state)
        if match is None:
            raise UnsettledScope("UNSETTLED Docker workload terminal state")
        return int(match[1])
    except BaseException as error:
        failure = error
        raise
    finally:
        cleanup_deadline = retirement_deadline()
        try:
            if not creation_started:
                SCOPE.settled(parent)
            elif container is None or not re.fullmatch(r"[0-9a-f]{64}", container):
                raise UnsettledScope(f"UNSETTLED Docker creation for {name}; retain its named resource for investigation") from failure
            if creation_started:
                retire(container, parent, attached, deadline=cleanup_deadline)
                attached = None
        finally:
            if attached is not None:
                attached.settle(deadline=cleanup_deadline)


def retire(container: str, parent: str, attached: OwnedProcess | None, *, deadline: float) -> None:
    control(["docker", "rm", "--force", container], parent, cleanup=True, deadline=deadline)
    remaining = control(["docker", "container", "ls", "--all", "--no-trunc",
                         "--filter", f"id={container}", "--format", "{{.ID}}"], parent, cleanup=True, deadline=deadline)
    if remaining.strip():
        raise UnsettledScope(f"UNSETTLED Docker removal: {container}")
    if attached is not None:
        attached.settle(deadline=deadline)
    SCOPE.settled(parent)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("name")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    try:
        return run(args.name, command)
    except ScopeCancelled:
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
