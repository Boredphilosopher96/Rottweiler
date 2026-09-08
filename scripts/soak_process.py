"""Physical closure of the soak's supervised, independently grouped product."""
from __future__ import annotations

import os
import select
import signal
import subprocess
import time
from collections.abc import Callable

from perf_process_scope import UnsettledScope
from perf_process_wait import observe_exit, observe_stopped, require_group_disappearance, signal_owned_group


def pid_exists(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def wait_with_terminal(process: subprocess.Popen[bytes], terminal: int, timeout: float) -> int | None:
    """Keep teardown output flowing without reaping the identity anchor."""
    deadline = time.monotonic() + timeout
    while True:
        status = observe_exit(process.pid)
        if status is not None or time.monotonic() >= deadline:
            return status
        if select.select([terminal], [], [], .01)[0]:
            try:
                os.read(terminal, 64 * 1024)
            except (BlockingIOError, OSError):
                pass


def terminate_supervisor(
    process: subprocess.Popen[bytes], terminal: int,
    observed: set[int], snapshot: Callable[[], set[int]], *, grace: float = 5,
) -> None:
    """Require product cleanup acknowledgement and observed group disappearance.

    Supervisor::run handles SIGTERM, awaits cleanup_managed_children, and returns
    success only after both managed children were reaped. Its zero exit therefore
    covers children launched between process snapshots. A killed or failed
    supervisor supplies no such proof. Observations are absence checks only;
    they never authorize signaling a historical PID or process group.
    """
    if process.returncode is not None:
        raise UnsettledScope(f"UNSETTLED supervisor {process.pid}: identity already reaped")
    failures: list[str] = []
    groups: set[int] = set()
    try:
        observed.update(snapshot())
        if len(observed) > 256:
            raise RuntimeError("soak process observation exceeds 256 children")
        for pid in observed:
            try:
                groups.add(os.getpgid(pid))
            except ProcessLookupError:
                pass
    except BaseException as error:
        failures.append(f"process observation: {str(error)[-1000:]}")
    initially_exited = observe_exit(process.pid) is not None
    status = None
    try:
        if not initially_exited:
            # The unreaped Popen leader is exact signal authority. Signal only
            # it first; its children have their own product cleanup ordering.
            os.kill(process.pid, signal.SIGTERM)
            status = wait_with_terminal(process, terminal, grace)
        else:
            status = observe_exit(process.pid)
        if initially_exited or status != 0:
            failures.append(f"missing successful supervisor cleanup: initial_exit={initially_exited}, status={status}")
        signal_owned_group(process.pid, signal.SIGKILL)
        process.wait(timeout=2)
        require_group_disappearance(process.pid)
    except BaseException as error:
        failures.append(f"anchored supervisor cleanup: {str(error)[-1000:]}")
    # Never signal after reaping, even if an observed group ID has been reused.
    # A conservative failure retains the private files for exact investigation.
    deadline = time.monotonic() + grace
    alive = sorted(observed - {process.pid})
    while alive and time.monotonic() < deadline:
        alive = [pid for pid in alive if pid_exists(pid)]
        if alive:
            time.sleep(.01)
    if alive:
        failures.append(f"observed processes still present: {alive}")
    for group in sorted(groups - {process.pid}):
        try:
            require_group_disappearance(group, timeout=max(0, deadline - time.monotonic()))
        except BaseException as error:
            failures.append(str(error)[-1000:])
    if failures:
        raise UnsettledScope(f"UNSETTLED soak supervisor {process.pid}: " + "; ".join(failures))


def kill_direct_child(
    supervisor: subprocess.Popen[bytes], select_child: Callable[[], int | None],
) -> int | None:
    """Pin a direct child's PID by stopping its unreaped parent during the fault.

    The selector must return an actual direct child from a fresh observation.
    The stopped parent cannot reap that child, so even its concurrent exit cannot
    recycle its PID before the fault signal. No historical PID is accepted.
    """
    if supervisor.returncode is not None or observe_exit(supervisor.pid) is not None:
        raise UnsettledScope("cannot pin a child of an exited supervisor")
    os.kill(supervisor.pid, signal.SIGSTOP)
    try:
        deadline = time.monotonic() + 2
        while not observe_stopped(supervisor.pid):
            if observe_exit(supervisor.pid) is not None or time.monotonic() >= deadline:
                raise UnsettledScope("supervisor did not stop before child fault")
            time.sleep(.001)
        pid = select_child()
        if pid is not None:
            os.kill(pid, signal.SIGKILL)
        return pid
    finally:
        # This Popen leader remains unreaped throughout, including selection or
        # signal failure. Resume it so its actual child owner can run cleanup.
        os.kill(supervisor.pid, signal.SIGCONT)
