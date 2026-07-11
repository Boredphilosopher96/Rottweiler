#!/usr/bin/env python3
"""Create a byte-reproducible Rottweiler release archive."""

from __future__ import annotations

import argparse
import gzip
import os
from pathlib import Path
import stat
import tarfile
import tempfile


DEFAULT_EPOCH = 1_700_000_000


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("stage", type=Path)
    parser.add_argument("archive", type=Path)
    parser.add_argument(
        "--epoch",
        type=int,
        default=int(os.environ.get("SOURCE_DATE_EPOCH", DEFAULT_EPOCH)),
    )
    return parser.parse_args()


def archive_name(stage: Path, path: Path) -> str:
    relative = path.relative_to(stage)
    if relative == Path("."):
        return stage.name
    return f"{stage.name}/{relative.as_posix()}"


def add_path(tar: tarfile.TarFile, stage: Path, path: Path, epoch: int) -> None:
    metadata = path.lstat()
    name = archive_name(stage, path)
    info = tarfile.TarInfo(name)
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    info.mtime = epoch

    if stat.S_ISDIR(metadata.st_mode):
        info.type = tarfile.DIRTYPE
        info.mode = 0o755
        info.size = 0
        tar.addfile(info)
        return
    if not stat.S_ISREG(metadata.st_mode):
        raise ValueError(f"release staging tree contains a non-regular entry: {path}")

    info.type = tarfile.REGTYPE
    info.mode = 0o755 if metadata.st_mode & 0o111 else 0o644
    info.size = metadata.st_size
    with path.open("rb") as source:
        tar.addfile(info, source)
    if path.stat().st_size != metadata.st_size:
        raise ValueError(f"release input changed while packaging: {path}")


def package(stage: Path, archive: Path, epoch: int) -> None:
    stage = stage.resolve(strict=True)
    if not stage.is_dir() or stage.name in {"", ".", ".."}:
        raise ValueError("stage must be a named directory")
    if epoch < 0:
        raise ValueError("SOURCE_DATE_EPOCH must be non-negative")

    archive.parent.mkdir(parents=True, exist_ok=True)
    entries = [stage, *sorted(stage.rglob("*"), key=lambda item: item.relative_to(stage).as_posix())]
    handle, temporary_name = tempfile.mkstemp(prefix=f".{archive.name}.", dir=archive.parent)
    temporary = Path(temporary_name)
    try:
        with os.fdopen(handle, "wb") as raw:
            with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch, compresslevel=9) as compressed:
                with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as tar:
                    for entry in entries:
                        add_path(tar, stage, entry, epoch)
            raw.flush()
            os.fsync(raw.fileno())
        temporary.chmod(0o644)
        os.replace(temporary, archive)
        directory = os.open(archive.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        temporary.unlink(missing_ok=True)


def main() -> None:
    args = parse_args()
    package(args.stage, args.archive, args.epoch)


if __name__ == "__main__":
    main()
