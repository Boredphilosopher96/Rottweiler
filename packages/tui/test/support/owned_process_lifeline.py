"""Cancel an owned test process when its parent VM loses the control pipe."""
from __future__ import annotations

import os
import select
import signal
import stat
import threading


class ParentLifeline:
    def __init__(self, descriptor: int):
        mode = os.fstat(descriptor).st_mode
        if not (stat.S_ISFIFO(mode) or stat.S_ISSOCK(mode)):
            raise ValueError("test supervisor requires an owned parent lifeline pipe")
        self.descriptor = descriptor
        self.stopped = threading.Event()
        self.thread = threading.Thread(target=self._watch, name="test-parent-lifeline")

    def _watch(self):
        while not self.stopped.is_set():
            try:
                readable, _, _ = select.select([self.descriptor], [], [], .05)
                if not readable:
                    continue
                # The pipe carries lifetime only, never commands. EOF or any
                # unexpected byte requests the same cooperative cancellation.
                os.read(self.descriptor, 1)
            except OSError:
                pass
            if not self.stopped.is_set():
                os.kill(os.getpid(), signal.SIGTERM)
            return

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.stopped.set()
        # select has a fixed bound; retaining and joining this actual thread
        # avoids leaving a watcher behind after successful native settlement.
        self.thread.join()
