"""Physical child/group ownership shared by measured samples and CI evidence."""
from __future__ import annotations

import contextlib
import os
from pathlib import Path
import signal
import subprocess
import time

from perf_process_deadline import COOPERATIVE_SECONDS, RETIREMENT_SECONDS, remaining
from perf_process_scope import SCOPE_FD, ScopeReader, UnsettledScope, inherited_scope
from perf_process_wait import observe_exit, signal_owned_group, require_group_disappearance

SCOPE = inherited_scope()


class OwnedProcess:
    """One unreaped leader anchors every real signal; closure requires actual proof."""

    def __init__(self, command: list[str], *, cwd: Path, env: dict[str, str],
                 delegated: bool = False, output: str = "capture",
                 cleanup_of: str | None = None, terminal: int | None = None):
        if output not in {"capture", "combined", "inherit", "stdout"}:
            raise ValueError("unknown physical process output mode")
        if terminal is not None and (output != "capture" or delegated):
            raise ValueError("a terminal uses captured stderr and a direct process owner")
        self.registration = SCOPE.starting(cleanup_of=cleanup_of)
        self.scope = None
        self.process = None
        self.finished = False
        self.failure = None
        writer = None
        environment = dict(env)
        environment.pop(SCOPE_FD, None)
        try:
            if delegated:
                descriptor, writer = os.pipe()
                os.set_blocking(writer, False)
                self.scope = ScopeReader(descriptor)
                os.set_blocking(descriptor, False)
                environment[SCOPE_FD] = str(writer)
            self.process = subprocess.Popen(
                command, cwd=cwd, env=environment, stdin=subprocess.DEVNULL if terminal is None else terminal,
                stdout=terminal if terminal is not None else (None if output == "inherit" else subprocess.PIPE),
                stderr=None if output in {"inherit", "stdout"} else (subprocess.STDOUT if output == "combined" else subprocess.PIPE),
                start_new_session=True, pass_fds=() if writer is None else (writer,),
            )
        except BaseException:
            if self.scope is not None:
                os.close(self.scope.descriptor)
            SCOPE.settled(self.registration)
            raise
        finally:
            if writer is not None:
                os.close(writer)
        try:
            for stream in (self.process.stdout, self.process.stderr):
                if stream is not None:
                    os.set_blocking(stream.fileno(), False)
            SCOPE.started(self.registration, self.process.pid)
        except BaseException:
            self.settle()
            raise

    def observe_exit(self) -> int | None:
        if self.finished:
            return self.process.returncode
        if self.failure is not None:
            raise self.failure
        return observe_exit(self.process.pid)

    def settle(self, *, deadline: float | None = None) -> None:
        if self.finished:
            return
        if self.failure is not None:
            raise self.failure
        process = self.process
        started = time.monotonic()
        deadline = min(deadline, started + RETIREMENT_SECONDS) if deadline is not None else started + RETIREMENT_SECONDS
        try:
            cooperative_settled = self.scope is None
            if self.scope is not None:
                signal_owned_group(process.pid, signal.SIGTERM, timeout=min(1, remaining(deadline)))
                settle_by = min(started + COOPERATIVE_SECONDS, deadline - (RETIREMENT_SECONDS - COOPERATIVE_SECONDS))
                while time.monotonic() < settle_by:
                    self.scope.drain()
                    if observe_exit(process.pid) is not None and self.scope.closed:
                        cooperative_settled = True
                        break
                    for stream in (process.stdout, process.stderr):
                        if stream is not None:
                            with contextlib.suppress(BlockingIOError):
                                os.read(stream.fileno(), 16 * 1024)
                    time.sleep(.01)
            # The leader remains waitable through the final real group signal.
            # After reap only absence checks are authorized, never another signal.
            signal_owned_group(process.pid, signal.SIGKILL, timeout=min(1, remaining(deadline)))
            process.wait(timeout=remaining(deadline))
            require_group_disappearance(process.pid, timeout=remaining(deadline))
            if self.scope is not None:
                self.scope.require_closed()
                if not cooperative_settled:
                    raise UnsettledScope("UNSETTLED delegated owner exceeded cooperative retirement deadline")
            SCOPE.settled(self.registration)
            self.finished = True
        except BaseException as error:
            self.failure = UnsettledScope(f"UNSETTLED owned process {process.pid}: {error}")
            raise self.failure from error
        finally:
            for stream in (process.stdout, process.stderr):
                if stream is not None:
                    stream.close()
            if self.scope is not None:
                os.close(self.scope.descriptor)
