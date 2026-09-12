"""Bounded M4 engine diagnostics retained until the physical stderr pipe settles."""
from __future__ import annotations

import os
from pathlib import Path
import select
import threading

from perf_process_scope import UnsettledScope

MAX_STDERR_BYTES = 2 * 1024 * 1024


class EngineErrorLog:
    def __init__(self, path: Path):
        self.path = path
        self.read_fd, self.write_fd = os.pipe()
        os.set_blocking(self.read_fd, False)
        self.stop = threading.Event()
        self.eof = False
        self.overflow = False
        self.failure: BaseException | None = None
        self.input_closed = False
        self.worker = threading.Thread(target=self._drain, name="m4-stderr", daemon=False)
        try:
            self.output = path.open('xb')
            self.worker.start()
        except BaseException:
            os.close(self.read_fd)
            os.close(self.write_fd)
            if hasattr(self, 'output'):
                self.output.close()
            raise

    def _drain(self):
        written = 0
        try:
            while not self.stop.is_set():
                ready, _, _ = select.select([self.read_fd], [], [], .05)
                if not ready:
                    continue
                block = os.read(self.read_fd, 64 * 1024)
                if not block:
                    self.eof = True
                    break
                retained = block[:max(0, MAX_STDERR_BYTES - written)]
                self.output.write(retained)
                self.output.flush()
                written += len(retained)
                if len(retained) != len(block):
                    self.overflow = True
        except BaseException as error:
            self.failure = error
        finally:
            os.close(self.read_fd)
            self.output.close()

    def close_input(self):
        if not self.input_closed:
            os.close(self.write_fd)
            self.input_closed = True

    def finish(self):
        self.close_input()
        self.worker.join(timeout=2)
        if self.worker.is_alive():
            self.stop.set()
            self.worker.join(timeout=2)
        if self.worker.is_alive() or not self.eof:
            raise UnsettledScope(f"UNSETTLED M4 stderr owner: {self.path}")
        if self.failure is not None:
            raise self.failure
        if self.overflow:
            raise ValueError(f"M4 engine stderr exceeded {MAX_STDERR_BYTES} bytes")
