"""Explicit physical-settlement acknowledgements for nested verification owners.

The supervisor never signals a delegated PID: only its actual Popen owner may
terminate it. Forced wrapper death is unproven settlement, even before the first
child-start announcement. That failure stops qualification.
"""
from __future__ import annotations

import atexit
import json
import os
import signal
import stat
import threading
from dataclasses import dataclass, field

SCOPE_FD = "RW_PERF_SETTLEMENT_FD"
MAX_ACTIVE = 128
MAX_RECORD = 512


class UnsettledScope(RuntimeError):
    """A wrapper exited without proving settlement of its physical children."""


class ScopeCancelled(RuntimeError):
    pass


@dataclass
class ScopeReader:
    descriptor: int
    pending: bytearray = field(default_factory=bytearray)
    active: dict[str, int | None] = field(default_factory=dict)
    closed: bool = False
    failure: str | None = None

    def append(self, chunk: bytes) -> None:
        for byte in chunk:
            if byte == 10:
                self._record(bytes(self.pending))
                self.pending.clear()
            elif len(self.pending) < MAX_RECORD:
                self.pending.append(byte)
            else:
                self.failure = "oversized settlement record"

    def _record(self, encoded: bytes) -> None:
        if self.failure is not None:
            return
        try:
            message = json.loads(encoded)
            kind = message["kind"]
            if self.closed:
                raise ValueError("record after scope closure")
            if kind == "closed":
                if set(message) != {"kind"} or self.active:
                    raise ValueError("scope closed with physical work pending")
                self.closed = True
                return
            token = message["token"]
            if not isinstance(token, str) or not 1 <= len(token) <= 64:
                raise ValueError("invalid settlement token")
            if kind == "starting":
                if set(message) != {"kind", "token"} or token in self.active or len(self.active) == MAX_ACTIVE:
                    raise ValueError("duplicate or excessive physical work")
                self.active[token] = None
            elif kind == "started":
                pid = message["pid"]
                if set(message) != {"kind", "token", "pid"} or token not in self.active or self.active[token] is not None or type(pid) is not int or pid <= 0:
                    raise ValueError("invalid child-start acknowledgement")
                self.active[token] = pid
            elif kind == "settled":
                if set(message) != {"kind", "token"} or token not in self.active:
                    raise ValueError("unowned settlement acknowledgement")
                del self.active[token]
            else:
                raise ValueError("unknown settlement record")
        except (KeyError, TypeError, ValueError):
            self.failure = "invalid settlement acknowledgement"

    def drain(self) -> None:
        for _ in range(16):
            try:
                data = os.read(self.descriptor, 4096)
            except BlockingIOError:
                return
            if not data:
                return
            self.append(data)

    def require_closed(self) -> None:
        self.drain()
        if self.failure or self.pending or not self.closed or self.active:
            raise UnsettledScope("UNSETTLED verification wrapper: " + json.dumps({
                "reason": self.failure or "missing physical-settlement acknowledgement",
                "active_children": self.active,
            }, sort_keys=True))


class ProcessScope:
    def __init__(self, descriptor: int | None):
        self.descriptor = descriptor
        self.counter = 0
        self.active: set[str] = set()
        self.failed = False
        self.cancelled = 0
        # The registration capability is for this Python owner, never its native
        # tools. Popen's default close_fds also prevents accidental inheritance.
        if descriptor is not None:
            os.set_inheritable(descriptor, False)
            os.set_blocking(descriptor, False)
        if threading.current_thread() is not threading.main_thread():
            raise RuntimeError("verification process ownership must initialize on its main thread")
        signal.signal(signal.SIGTERM, self._cancel)
        signal.signal(signal.SIGINT, self._cancel)
        atexit.register(self._close)

    def _cancel(self, number, _frame):
        # Popen must finish transferring its actual child before cancellation is
        # observed. Raising in this handler would reopen the spawn handoff gap.
        self.cancelled = number

    def check(self) -> None:
        if self.cancelled:
            raise ScopeCancelled(f"verification cancelled by signal {self.cancelled}")

    def _send(self, message: dict) -> None:
        if self.descriptor is None:
            return
        data = json.dumps(message, separators=(",", ":")).encode() + b"\n"
        try:
            if len(data) > MAX_RECORD or os.write(self.descriptor, data) != len(data):
                raise RuntimeError("settlement registration was not atomic")
        except BaseException:
            self.failed = True
            raise

    def starting(self) -> str:
        self.check()
        if len(self.active) == MAX_ACTIVE:
            raise RuntimeError("too many active verification children")
        self.counter += 1
        token = f"{os.getpid()}:{self.counter}"
        self.active.add(token)
        self._send({"kind": "starting", "token": token})
        return token

    def started(self, token: str, pid: int) -> None:
        self._send({"kind": "started", "token": token, "pid": pid})

    def settled(self, token: str) -> None:
        self.active.remove(token)
        self._send({"kind": "settled", "token": token})

    def _close(self) -> None:
        if not self.failed and not self.active:
            try:
                self._send({"kind": "closed"})
            except (OSError, RuntimeError):
                pass
        if self.descriptor is not None:
            os.close(self.descriptor)


def inherited_scope() -> ProcessScope:
    raw = os.environ.pop(SCOPE_FD, None)
    if raw is None:
        return ProcessScope(None)
    descriptor = int(raw)
    if descriptor < 3 or not stat.S_ISFIFO(os.fstat(descriptor).st_mode):
        raise ValueError("verification settlement descriptor is not an owned pipe")
    return ProcessScope(descriptor)
