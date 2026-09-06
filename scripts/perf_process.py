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


def run_sample(
    command: list[str], *, cwd: Path, env: dict[str, str],
    timeout: float = 5.0, output_limit: int = 64 * 1024,
) -> subprocess.CompletedProcess[bytes]:
    """Drain both pipes within fixed budgets and reap the child on every path."""
    if not math.isfinite(timeout) or timeout <= 0 or output_limit <= 0:
        raise ValueError("sample time and output budgets must be positive")
    started = time.monotonic()
    deadline = started + timeout
    process = subprocess.Popen(
        command, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
    )
    spawn_ms = (time.monotonic() - started) * 1000
    stdout, stderr = bytearray(), bytearray()

    def deadline_error(pending: int) -> TimeoutError:
        status = process.poll()
        leader = "running" if status is None else f"exited:{status}"
        return TimeoutError(
            f"performance sample exceeded {timeout:g}s "
            f"(spawn_ms={spawn_ms:.3f}, leader={leader}, pending_pipes={pending}, "
            f"stdout_bytes={len(stdout)}, stderr_bytes={len(stderr)})"
        )

    try:
        with selectors.DefaultSelector() as selector:
            for stream, captured in ((process.stdout, stdout), (process.stderr, stderr)):
                assert stream is not None
                os.set_blocking(stream.fileno(), False)
                selector.register(stream, selectors.EVENT_READ, captured)
            while selector.get_map():
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
                        chunk = os.read(key.fd, min(16 * 1024, output_limit + 1 - len(key.data)))
                    except BlockingIOError:
                        continue
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    key.data.extend(chunk)
                    if len(key.data) > output_limit:
                        raise ValueError(f"performance sample exceeded {output_limit} output bytes per stream")
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise deadline_error(0)
            try:
                returncode = process.wait(timeout=remaining)
            except subprocess.TimeoutExpired as error:
                raise deadline_error(0) from error
        return subprocess.CompletedProcess(command, returncode, bytes(stdout), bytes(stderr))
    finally:
        # Descendants can retain pipe descriptors after the leader exits. The
        # sample owns its process group even when output or a deadline fails.
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGKILL)
        try:
            process.wait(timeout=5)
        finally:
            for stream in (process.stdout, process.stderr):
                if stream is not None:
                    stream.close()
