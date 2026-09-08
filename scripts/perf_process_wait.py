"""Observe a child exit without releasing its PID/process-group identity."""
from __future__ import annotations

import ctypes
import errno
import os
import sys


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


if sys.platform == "darwin":
    _libproc = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)
    _group_members = _libproc.proc_listpgrppids
    _group_members.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
    _group_members.restype = ctypes.c_int


def observe_exit(pid: int) -> int | None:
    """Return the exit code, leaving this exact child waitable until final reap."""
    options = os.WEXITED | os.WNOHANG | os.WNOWAIT
    if hasattr(os, "waitid"):
        result = os.waitid(os.P_PID, pid, options)
        if result is None:
            return None
        return result.si_status if result.si_code == os.CLD_EXITED else -result.si_status
    if sys.platform != "darwin":
        raise RuntimeError("sample settlement requires waitid with WNOWAIT")
    result = DarwinSignalInfo()
    while _waitid(os.P_PID, pid, ctypes.byref(result), options) != 0:
        error = ctypes.get_errno()
        if error != errno.EINTR:
            raise OSError(error, os.strerror(error))
    if result.pid == 0:
        return None
    return result.status if result.code == os.CLD_EXITED else -result.status


def signal_owned_group(pid: int, number: int) -> None:
    """Signal while the owned leader is unreaped, including a zombie-only group."""
    try:
        os.killpg(pid, number)
    except ProcessLookupError:
        return
    except PermissionError:
        # XNU killpg1 skips SZOMB members and returns EPERM if none is eligible.
        # Do not mask an actual permission denial: prove this anchored group
        # contains only our already-exited child with a bounded libproc query.
        if sys.platform != "darwin" or observe_exit(pid) is None:
            raise
        members = (ctypes.c_int * 2)()
        count = _group_members(pid, members, ctypes.sizeof(members))
        if count != 1 or members[0] != pid:
            raise


def require_group_disappearance(pid: int, timeout: float = 5) -> None:
    """Prove no process remains in the group; never signal after leader reaping."""
    import time
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
