"""Observe a child exit without releasing its PID/process-group identity."""
from __future__ import annotations

import ctypes
import errno
import os
import sys
import time


if sys.platform == "darwin" and not hasattr(os, "waitid"):
    class DarwinSignalInfo(ctypes.Structure):
        # Darwin SDK sys/signal.h siginfo_t, LP64 ABI. This is not the Linux
        # siginfo_t layout; Linux uses Python's native os.waitid binding below.
        _fields_ = [("signo", ctypes.c_int), ("error", ctypes.c_int),
                    ("code", ctypes.c_int), ("pid", ctypes.c_int),
                    ("uid", ctypes.c_uint), ("status", ctypes.c_int),
                    ("address", ctypes.c_void_p), ("value", ctypes.c_void_p),
                    ("band", ctypes.c_long), ("reserved", ctypes.c_ulong * 7)]

    if (ctypes.sizeof(DarwinSignalInfo), ctypes.alignment(DarwinSignalInfo),
        DarwinSignalInfo.pid.offset, DarwinSignalInfo.status.offset, DarwinSignalInfo.code.offset) != (104, 8, 12, 20, 8):
        raise RuntimeError("unsupported Darwin waitid ABI")
    _libc = ctypes.CDLL(None, use_errno=True)
    _waitid = _libc.waitid
    _waitid.argtypes = [ctypes.c_int, ctypes.c_uint, ctypes.POINTER(DarwinSignalInfo), ctypes.c_int]
    _waitid.restype = ctypes.c_int


def _observe(pid: int, events: int) -> tuple[int, int] | None:
    """Return code/status without consuming this owner's child identity."""
    options = events | os.WNOHANG | os.WNOWAIT
    if hasattr(os, "waitid"):
        result = os.waitid(os.P_PID, pid, options)
        return None if result is None else (result.si_code, result.si_status)
    if sys.platform != "darwin":
        raise RuntimeError("sample settlement requires waitid with WNOWAIT")
    result = DarwinSignalInfo()
    while _waitid(os.P_PID, pid, ctypes.byref(result), options) != 0:
        error = ctypes.get_errno()
        if error != errno.EINTR:
            raise OSError(error, os.strerror(error))
    return None if result.pid == 0 else (result.code, result.status)


def observe_exit(pid: int) -> int | None:
    """Return the exit code, leaving this exact child waitable until final reap."""
    result = _observe(pid, os.WEXITED)
    if result is None:
        return None
    code, status = result
    return status if code == os.CLD_EXITED else -status


def observe_stopped(pid: int) -> bool:
    """Prove an owned child stopped, retaining both stop and exit wait state."""
    result = _observe(pid, os.WSTOPPED | os.WEXITED)
    return result is not None and result[0] == os.CLD_STOPPED


def signal_owned_group(pid: int, number: int) -> None:
    """Signal an unreaped leader's group; callers must then reap and prove absence."""
    try:
        os.killpg(pid, number)
    except ProcessLookupError:
        return
    except PermissionError:
        if sys.platform != "darwin":
            raise
        # XNU can reject the signal while exit is in progress, before waitid
        # publishes the status. Keep the PID owned through that bounded handoff.
        # An exited leader is not group settlement: every caller still reaps it
        # and requires group disappearance. Live descendants cannot earn an ack.
        deadline = time.monotonic() + 1
        while observe_exit(pid) is None:
            if time.monotonic() >= deadline:
                raise
            time.sleep(.001)


def require_group_disappearance(pid: int, timeout: float = 5) -> None:
    """Prove no process remains in the group; never signal after leader reaping."""
    from perf_process_scope import UnsettledScope
    deadline = time.monotonic() + timeout
    while True:
        try:
            os.killpg(pid, 0)
        except ProcessLookupError:
            return
        except PermissionError:
            # A zombie-only Darwin group or a reused/unowned group is not a
            # disappearance proof. Neither may receive another actual signal.
            pass
        if time.monotonic() >= deadline:
            raise UnsettledScope(f"UNSETTLED process group: leader={pid} phase=after-reap")
        time.sleep(.001)
