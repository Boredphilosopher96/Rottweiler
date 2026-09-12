"""Finite async observation over the existing physical process owner."""
from __future__ import annotations
import asyncio
import contextlib
import errno
import os
from pathlib import Path
import pty
import time
from perf_process import check_sample_cancellation
from perf_process_owner import OwnedProcess

LIMIT = 2 * 1024 * 1024


class NativeProcess:
    def __init__(self, command, *, cwd: Path, env: dict, log: Path, interactive=False):
        self.master = None
        self.slave = None
        self.owner = None
        self.output = bytearray()
        self.log = log.open("xb")
        self.log_bytes = 0
        self.pipes = []
        try:
            if interactive:
                self.master, self.slave = pty.openpty()
                os.set_blocking(self.master, False)
            self.owner = OwnedProcess(command, cwd=cwd, env=env, terminal=self.slave)
            self.pipes = [pipe.fileno() for pipe in (self.owner.process.stdout, self.owner.process.stderr) if pipe]
            if self.master is not None:
                self.pipes.append(self.master)
        except BaseException:
            self.close()
            raise
        finally:
            if self.slave is not None:
                os.close(self.slave)
                self.slave = None

    def drain(self):
        check_sample_cancellation()
        for descriptor in list(self.pipes):
            try:
                block = os.read(descriptor, min(16 * 1024, LIMIT + 1 - self.log_bytes))
            except BlockingIOError:
                continue
            except OSError as error:
                if descriptor == self.master and error.errno == errno.EIO:
                    block = b""
                else:
                    raise
            if not block:
                self.pipes.remove(descriptor)
                continue
            self.log.write(block[:LIMIT - self.log_bytes])
            self.log.flush()
            self.log_bytes += len(block)
            if self.log_bytes > LIMIT:
                raise ValueError("native fixture output exceeds 2 MiB")
            # Only approval needs text inspection; all evidence is already in the file.
            self.output.extend(block)
            if len(self.output) > 128 * 1024:
                del self.output[:-128 * 1024]

    async def finish(self, seconds: float, *, approve=False, companion=None):
        deadline = time.monotonic() + seconds
        confirmed = False
        while True:
            self.drain()
            if companion is not None:
                companion.drain()
                if companion.owner.observe_exit() is not None:
                    raise RuntimeError("native engine exited before UI settlement")
            if approve and not confirmed and b"Approve this exact plugin identity? [y/N]" in self.output:
                os.write(self.master, b"yes\n")
                confirmed = True
            code = self.owner.observe_exit()
            if code is not None and not self.pipes:
                if code or (approve and not confirmed):
                    raise RuntimeError(f"native fixture process failed with status {code}")
                return
            if time.monotonic() >= deadline:
                raise TimeoutError("native fixture process exceeded its deadline")
            await asyncio.sleep(.01)

    def close(self):
        try:
            if self.owner is not None:
                self.owner.settle()
        finally:
            for descriptor in (self.master, self.slave):
                if descriptor is not None:
                    with contextlib.suppress(OSError):
                        os.close(descriptor)
            self.master = self.slave = None
            self.log.close()
