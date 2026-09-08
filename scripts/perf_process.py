"""Bounded process ownership for native performance samples."""
from __future__ import annotations

import contextlib
import math
import os
from pathlib import Path
import selectors
import subprocess
import time
from typing import BinaryIO

from perf_process_scope import ScopeReader
from perf_process_owner import OwnedProcess, SCOPE as _SCOPE
from perf_process_wait import observe_exit



def require_sample_settlement() -> None:
    """Check physical closure before transferring a sample result to its caller."""
    _SCOPE.require_settled()


def check_sample_cancellation() -> None:
    _SCOPE.check()


@contextlib.contextmanager
def delegated_success_scope():
    """A trusted gate acknowledges its raw owners only after successful closure.

    Any failure leaves the obligation pending, even if some cleanup completed.
    The outer supervisor must classify that run as UNSETTLED.
    """
    registration = _SCOPE.starting()
    yield
    _SCOPE.settled(registration)


def wait_between_samples(seconds: float) -> None:
    """Fixed conditioning interval, interruptible without abandoning scratch."""
    if not math.isfinite(seconds) or seconds < 0:
        raise ValueError("sample conditioning interval must be finite and nonnegative")
    deadline = time.monotonic() + seconds
    while True:
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
    owner = OwnedProcess(command, cwd=cwd, env=env, delegated=delegated)
    process, scope = owner.process, owner.scope
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
        with selectors.DefaultSelector() as selector:
            for stream, captured in ((process.stdout, stdout), (process.stderr, stderr)):
                assert stream is not None
                os.set_blocking(stream.fileno(), False)
                selector.register(stream, selectors.EVENT_READ, captured)
            if scope is not None:
                selector.register(scope.descriptor, selectors.EVENT_READ, scope)
            while selector.get_map():
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
        owner.settle()
