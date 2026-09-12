"""Best-effort nonblocking console projection of the retained CI evidence."""
from __future__ import annotations

import os


class ConsoleRelay:
    """Never queue console output behind a stalled downstream reader."""

    def __init__(self, descriptor: int):
        self.descriptor = descriptor
        self.blocking = os.get_blocking(descriptor)
        os.set_blocking(descriptor, False)
        self.omitted = 0
        self.failure = None

    def write(self, data: bytes) -> None:
        try:
            written = os.write(self.descriptor, data)
        except BlockingIOError:
            written = 0
        except OSError as error:
            written = 0
            self.failure = f"{type(error).__name__}: {error}"[:512]
        self.omitted += len(data) - written

    def close(self) -> None:
        os.set_blocking(self.descriptor, self.blocking)
