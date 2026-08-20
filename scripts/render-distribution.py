#!/usr/bin/env python3
"""Render pinned Homebrew packages and bootstrap from exact release archives."""

from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path
import re
import tempfile


REPO = Path(__file__).resolve().parents[1]
FORMULA_TEMPLATE = REPO / "packaging/homebrew/rottweiler.rb.in"
CASK_TEMPLATE = REPO / "packaging/homebrew/rottweiler.cask.rb.in"
BOOTSTRAP_TEMPLATE = REPO / "packaging/bootstrap/install.sh.in"
SUPPORTED = {
    "darwin-arm64": ("Darwin-arm64", "macos", "Hardware::CPU.arm?"),
    "darwin-x86_64": ("Darwin-x86_64", "macos", "Hardware::CPU.intel?"),
    "linux-arm64": ("Linux-aarch64", "linux", "Hardware::CPU.arm?"),
    "linux-x86_64": ("Linux-x86_64", "linux", "Hardware::CPU.intel? && Hardware::CPU.is_64_bit?"),
}
VERSION_PATTERN = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?")
REPOSITORY_PATTERN = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--repository", default="Boredphilosopher96/Rottweiler")
    parser.add_argument(
        "--archive",
        action="append",
        required=True,
        metavar="PLATFORM=PATH",
        help="exact release archive; repeat once per published platform",
    )
    parser.add_argument("--formula", required=True, type=Path)
    parser.add_argument("--cask", required=True, type=Path)
    parser.add_argument("--bootstrap", required=True, type=Path)
    return parser.parse_args()


def load_archives(values: list[str], version: str) -> dict[str, Path]:
    archives: dict[str, Path] = {}
    for value in values:
        platform, separator, supplied = value.partition("=")
        if not separator or platform not in SUPPORTED or platform in archives:
            raise ValueError(f"invalid or duplicate release archive binding: {value}")
        path = Path(supplied)
        metadata = path.lstat()
        if path.is_symlink() or not path.is_file() or metadata.st_nlink != 1:
            raise ValueError(f"release archive must be a single-link regular file: {path}")
        expected = f"rottweiler-{version}-{platform}.tar.gz"
        if path.name != expected:
            raise ValueError(f"release archive must be named {expected}: {path}")
        archives[platform] = path.resolve(strict=True)
    operating_systems = {SUPPORTED[platform][1] for platform in archives}
    if operating_systems != {"macos", "linux"}:
        raise ValueError("distribution rendering requires at least one macOS and one Linux archive")
    return archives


def artifact_metadata(path: Path) -> tuple[int, str]:
    digest = hashlib.sha256()
    length = 0
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
            length += len(chunk)
    if length == 0:
        raise ValueError(f"release archive must not be empty: {path}")
    if path.stat().st_size != length:
        raise ValueError(f"release archive changed while hashing: {path}")
    return length, digest.hexdigest()


def release_url(repository: str, version: str, path: Path) -> str:
    return f"https://github.com/{repository}/releases/download/v{version}/{path.name}"


def render_formula(repository: str, version: str, archives: dict[str, Path]) -> str:
    by_os: dict[str, list[str]] = {"macos": [], "linux": []}
    for platform in sorted(archives):
        _, operating_system, condition = SUPPORTED[platform]
        _, digest = artifact_metadata(archives[platform])
        by_os[operating_system].append(
            "    {keyword} {condition}\n"
            "      url \"{url}\"\n"
            "      sha256 \"{digest}\"".format(
                keyword="if" if not by_os[operating_system] else "elsif",
                condition=condition,
                url=release_url(repository, version, archives[platform]),
                digest=digest,
            )
        )

    blocks: list[str] = []
    for operating_system, label in (("macos", "macOS"), ("linux", "Linux")):
        choices = by_os[operating_system]
        if not choices:
            continue
        blocks.append(
            f"  on_{operating_system} do\n"
            + "\n".join(choices)
            + f"\n    else\n      odie \"Rottweiler does not publish a {label} bundle for this CPU\"\n"
            "    end\n  end"
        )

    return render_template(
        FORMULA_TEMPLATE,
        {
            "@REPOSITORY@": repository,
            "@VERSION@": version,
            "@PLATFORM_BLOCKS@": "\n\n".join(blocks),
        },
    )


def render_cask(repository: str, version: str, archives: dict[str, Path]) -> str:
    archive = archives.get("darwin-arm64")
    if archive is None:
        raise ValueError("Homebrew Cask rendering requires the darwin-arm64 archive")
    _, digest = artifact_metadata(archive)
    return render_template(
        CASK_TEMPLATE,
        {
            "@DARWIN_ARM64_URL@": release_url(repository, version, archive),
            "@HOMEPAGE_REPOSITORY@": repository,
            "@VERSION@": version,
            "@DARWIN_ARM64_SHA256@": digest,
        },
    )


def render_bootstrap(repository: str, version: str, archives: dict[str, Path]) -> str:
    cases: list[str] = []
    for platform in sorted(archives):
        uname, _, _ = SUPPORTED[platform]
        length, digest = artifact_metadata(archives[platform])
        root = f"rottweiler-{version}-{platform}"
        cases.append(
            f"  {uname})\n"
            f"    archive_url='{release_url(repository, version, archives[platform])}'\n"
            f"    archive_length='{length}'\n"
            f"    archive_sha256='{digest}'\n"
            f"    release_root='{root}'\n"
            "    ;;"
        )
    return render_template(
        BOOTSTRAP_TEMPLATE,
        {"@VERSION@": version, "@PLATFORM_CASES@": "\n".join(cases)},
    )


def render_template(path: Path, replacements: dict[str, str]) -> str:
    rendered = path.read_text(encoding="utf-8")
    for marker, value in replacements.items():
        if rendered.count(marker) != 1:
            raise ValueError(f"template marker must occur exactly once: {marker}")
        rendered = rendered.replace(marker, value)
    if re.search(r"@[A-Z][A-Z_]+@", rendered):
        raise ValueError(f"template contains an unresolved marker: {path}")
    return rendered


def write_atomic(path: Path, content: str, mode: int) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="\n") as output:
            output.write(content)
            output.flush()
            os.fsync(output.fileno())
        temporary.chmod(mode)
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def main() -> None:
    args = parse_args()
    if VERSION_PATTERN.fullmatch(args.version) is None:
        raise ValueError("version must be a canonical semantic version without a leading v")
    if REPOSITORY_PATTERN.fullmatch(args.repository) is None:
        raise ValueError("repository must be a GitHub OWNER/NAME pair")
    archives = load_archives(args.archive, args.version)
    write_atomic(args.formula, render_formula(args.repository, args.version, archives), 0o644)
    write_atomic(args.cask, render_cask(args.repository, args.version, archives), 0o644)
    write_atomic(args.bootstrap, render_bootstrap(args.repository, args.version, archives), 0o755)


if __name__ == "__main__":
    main()
