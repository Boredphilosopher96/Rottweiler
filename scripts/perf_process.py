"""Bounded process ownership for native performance samples."""
from __future__ import annotations

import contextlib
import math
import os
from pathlib import Path
import selectors
import signal
import subprocess
import time
from typing import BinaryIO

from perf_process_scope import SCOPE_FD, ScopeReader, inherited_scope
from perf_process_wait import observe_exit, signal_owned_group, require_group_disappearance

_SCOPE = inherited_scope()


def wait_between_samples(seconds: float) -> None:
    """Fixed conditioning interval, interruptible without abandoning scratch."""
    if not math.isfinite(seconds) or seconds < 0:
        raise ValueError("sample conditioning interval must be finite and nonnegative")
    deadline = time.monotonic() + seconds
    while True:
        if _SCOPE is not None:
            _SCOPE.check()
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return
        time.sleep(min(remaining, .05))


def run_sample(
    command: list[str], *, cwd: Path, env: dict[str, str],
    timeout: float = 5.0, output_limit: int = 64 * 1024,
    log: BinaryIO | None = None, delegated: bool = False,
) -> subprocess.CompletedProcess[bytes]:
    """Drain both pipes within fixed budgets and reap the child on every path."""
    if not math.isfinite(timeout) or timeout <= 0 or output_limit <= 0:
        raise ValueError("sample time and output budgets must be positive")
    started = time.monotonic()
    deadline = started + timeout
    registration = _SCOPE.starting() if _SCOPE is not None else None
    scope = None
    writer = None
    environment = dict(env)
    environment.pop(SCOPE_FD, None)
    try:
        if delegated:
            descriptor, writer = os.pipe()
            scope = ScopeReader(descriptor)
            os.set_blocking(descriptor, False)
            environment[SCOPE_FD] = str(writer)
        process = subprocess.Popen(
            command, cwd=cwd, env=environment, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
            pass_fds=() if writer is None else (writer,),
        )
    except BaseException:
        if scope is not None:
            os.close(scope.descriptor)
        if registration is not None:
            _SCOPE.settled(registration)
        raise
    finally:
        if writer is not None:
            os.close(writer)
    spawn_ms = (time.monotonic() - started) * 1000
    stdout, stderr = bytearray(), bytearray()

    def deadline_error(pending: int) -> TimeoutError:
        status = observe_exit(process.pid)
        leader = "running" if status is None else f"exited:{status}"
        return TimeoutError(
            f"performance sample exceeded {timeout:g}s "
            f"(spawn_ms={spawn_ms:.3f}, leader={leader}, pending_pipes={pending}, "
            f"stdout_bytes={len(stdout)}, stderr_bytes={len(stderr)})"
        )

    try:
        for stream in (process.stdout, process.stderr):
            assert stream is not None
            os.set_blocking(stream.fileno(), False)
        if registration is not None:
            _SCOPE.started(registration, process.pid)
        with selectors.DefaultSelector() as selector:
            for stream, captured in ((process.stdout, stdout), (process.stderr, stderr)):
                assert stream is not None
                os.set_blocking(stream.fileno(), False)
                selector.register(stream, selectors.EVENT_READ, captured)
            if scope is not None:
                selector.register(scope.descriptor, selectors.EVENT_READ, scope)
            while selector.get_map():
                if _SCOPE is not None:
                    _SCOPE.check()
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise deadline_error(len(selector.get_map()))
                ready = selector.select(min(remaining, .05))
                # Nonblocking reads establish EOF even if a platform readiness
                # notification is delayed or coalesced. The same deadline and
                # output budgets apply; an inherited open pipe still times out.
                candidates = [key for key, _ in ready] if ready else list(selector.get_map().values())
                for key in candidates:
                    try:
                        chunk = os.read(key.fd, 4096 if isinstance(key.data, ScopeReader) else min(16 * 1024, output_limit + 1 - len(key.data)))
                    except BlockingIOError:
                        continue
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    if isinstance(key.data, ScopeReader):
                        key.data.append(chunk)
                        continue
                    if log is not None:
                        log.write(chunk[:max(0, output_limit - len(key.data))])
                        log.flush()
                    key.data.extend(chunk)
                    if len(key.data) > output_limit:
                        raise ValueError(f"performance sample exceeded {output_limit} output bytes per stream")
            while True:
                if _SCOPE is not None:
                    _SCOPE.check()
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise deadline_error(0)
                returncode = observe_exit(process.pid)
                if returncode is not None:
                    break
                time.sleep(min(remaining, .001))
        return subprocess.CompletedProcess(command, returncode, bytes(stdout), bytes(stderr))
    finally:
        try:
            # A cooperative wrapper must let its actual Popen owners settle their
            # separately grouped children before it exits. Never signal a delegated
            # PID from a registration: that PID may already have been reused.
            if scope is not None and observe_exit(process.pid) is None:
                signal_owned_group(process.pid, signal.SIGTERM)
                settle_by = time.monotonic() + 5
                while observe_exit(process.pid) is None and time.monotonic() < settle_by:
                    scope.drain()
                    for stream in (process.stdout, process.stderr):
                        if stream is not None and not os.get_blocking(stream.fileno()):
                            with contextlib.suppress(BlockingIOError):
                                os.read(stream.fileno(), 16 * 1024)
                    time.sleep(.01)
            # WNOWAIT keeps the leader unreaped: its PID anchors this exact group
            # until the last signal. Never signal a group after releasing that PID.
            signal_owned_group(process.pid, signal.SIGKILL)
            process.wait(timeout=5)
            require_group_disappearance(process.pid)
            if scope is not None:
                scope.require_closed()
            if registration is not None:
                _SCOPE.settled(registration)
        finally:
            for stream in (process.stdout, process.stderr):
                if stream is not None:
                    stream.close()
            if scope is not None:
                os.close(scope.descriptor)
