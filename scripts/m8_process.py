"""Bounded terminal transport using the shared unreaped verification owner."""
from __future__ import annotations

import errno
import os
from pathlib import Path
import pty
import select
import time

from perf_process import check_sample_cancellation
from perf_process_owner import OwnedProcess

OUTPUT_BYTES = 4 * 1024 * 1024


class Terminal:
    def __init__(self, command: list[str], *, cwd: Path, env: dict[str, str]):
        self.master, slave = pty.openpty()
        self.owner = None
        self.closed = False
        try:
            os.set_blocking(self.master, False)
            self.spawn_started_ns = time.perf_counter_ns()
            self.owner = OwnedProcess(command, cwd=cwd, env=env, terminal=slave)
        except BaseException:
            os.close(self.master)
            raise
        finally:
            os.close(slave)
        self.stderr = self.owner.process.stderr
        self._readers = {self.master: 0, self.stderr.fileno(): 1}

    @property
    def pid(self) -> int:
        return self.owner.process.pid

    def observe_exit(self) -> int | None:
        check_sample_cancellation()
        return self.owner.observe_exit()

    def read(self, timeout: float = .01) -> tuple[bytes, bytes]:
        check_sample_cancellation()
        if not self._readers:
            time.sleep(timeout)
            check_sample_cancellation()
            return b"", b""
        ready, _, _ = select.select(list(self._readers), [], [], timeout)
        result = [b"", b""]
        for descriptor in ready:
            try:
                chunk = os.read(descriptor, 64 * 1024)
            except BlockingIOError:
                continue
            except OSError as error:
                if descriptor != self.master or error.errno != errno.EIO:
                    raise
                chunk = b""
            result[self._readers[descriptor]] = chunk
            if not chunk:
                del self._readers[descriptor]
        return result[0], result[1]

    def write(self, body: bytes, *, deadline: float) -> None:
        view = memoryview(body)
        while view:
            check_sample_cancellation()
            if time.monotonic() >= deadline:
                raise TimeoutError("M8 terminal input deadline")
            try:
                written = os.write(self.master, view)
                view = view[written:]
            except BlockingIOError:
                select.select([], [self.master], [], min(.01, max(0, deadline - time.monotonic())))

    def close(self) -> None:
        if self.closed:
            if self.owner.failure is not None:
                raise self.owner.failure
            return
        try:
            self.owner.settle()
        finally:
            os.close(self.master)
            self.closed = True


def append_bounded(buffer: bytearray, chunk: bytes) -> None:
    if len(buffer) + len(chunk) > OUTPUT_BYTES:
        raise ValueError("M8 terminal output exceeded 4 MiB evidence bound")
    buffer.extend(chunk)
