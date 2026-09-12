"""Private probe storage retained whenever process or oracle closure fails."""
from __future__ import annotations

from collections.abc import Callable, Iterator
from contextlib import contextmanager
from pathlib import Path
import shutil
import tempfile


@contextmanager
def retained_scratch(prefix: str, *, parent: Path, evidence: Callable[[Path], None]) -> Iterator[Path]:
    directory = Path(tempfile.mkdtemp(prefix=prefix, suffix=".noindex", dir=parent))
    try:
        yield directory
        shutil.rmtree(directory)
    except BaseException:
        # A failed process call may still own descendants. Cleanup belongs to
        # an identity-qualified investigation, never an unconditional temp exit.
        evidence(directory)
        raise
