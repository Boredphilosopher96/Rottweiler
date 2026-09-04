#!/usr/bin/env python3
"""Verify that build-toolchain projections match their repository owners."""

from __future__ import annotations

import json
from pathlib import Path
import re
import sys
import tomllib


sys.path.insert(0, str(Path(__file__).resolve().parent))
from ci_inventory import package_manifests

ROOT = Path(__file__).resolve().parents[1]
SEMVER = r"[0-9]+\.[0-9]+\.[0-9]+"


def _read_owners(root: Path) -> tuple[str, str]:
    with (root / "rust-toolchain.toml").open("rb") as stream:
        document = tomllib.load(stream)
    rust = document.get("toolchain", {}).get("channel")
    if not isinstance(rust, str) or re.fullmatch(SEMVER, rust) is None:
        raise ValueError("rust-toolchain.toml must own an exact semantic version")
    bun = (root / ".bun-version").read_text(encoding="utf-8").strip()
    if re.fullmatch(SEMVER, bun) is None:
        raise ValueError(".bun-version must own an exact semantic version")
    return rust, bun


def validate_repository(root: Path) -> list[str]:
    failures: list[str] = []
    try:
        rust, bun = _read_owners(root)
    except (OSError, tomllib.TOMLDecodeError, ValueError) as error:
        return [str(error)]

    workflow_root = root / ".github" / "workflows"
    workflow_files = sorted(workflow_root.glob("*.yml")) + sorted(workflow_root.glob("*.yaml"))
    for path in workflow_files:
        source = path.read_text(encoding="utf-8")
        for match in re.finditer(r"rustup (?:toolchain install|override set) (" + SEMVER + r")", source):
            if match.group(1) != rust:
                failures.append(f"{path.relative_to(root)} uses Rust {match.group(1)}; owner is {rust}")
        for match in re.finditer(r"\bbun-version:\s*('?)(" + SEMVER + r")\1", source):
            if match.group(2) != bun:
                failures.append(f"{path.relative_to(root)} uses Bun {match.group(2)}; owner is {bun}")

    for relative in (
        "scripts/wsl-acceptance.sh",
        "crates/rw-cli/tests/m8_release_gate_linux.sh",
        "crates/rw-sandbox/tests/linux_security_gate.sh",
    ):
        path = root / relative
        source = path.read_text(encoding="utf-8")
        versions = re.findall(r"(?:rustup (?:toolchain install|override set) |rust:)(" + SEMVER + r")", source)
        for version in versions:
            if version != rust:
                failures.append(f"{relative} uses Rust {version}; owner is {rust}")

    provision = (root / "scripts/provision-wsl-ci.sh").read_text(encoding="utf-8")
    provision_versions = re.findall(r"\bbun-v(" + SEMVER + r")\b", provision)
    if provision_versions != [bun]:
        failures.append("scripts/provision-wsl-ci.sh must project the root Bun version exactly once")

    package_paths = package_manifests(root)
    for relative in package_paths:
        document = json.loads((root / relative).read_text(encoding="utf-8"))
        if document.get("packageManager") != f"bun@{bun}":
            failures.append(f"{relative} packageManager does not project Bun {bun}")
        if document.get("engines", {}).get("bun") != bun:
            failures.append(f"{relative} engines.bun does not project Bun {bun}")

    tui_projection = (root / "packages/tui/.bun-version").read_text(encoding="utf-8").strip()
    if tui_projection != bun:
        failures.append(f"packages/tui/.bun-version uses Bun {tui_projection}; owner is {bun}")

    readme = (root / "README.md").read_text(encoding="utf-8")
    required = f"Source builds require Rust {rust} and Bun {bun}."
    if required not in readme:
        failures.append("README.md does not project the owned Rust and Bun versions")
    return failures


def main() -> int:
    failures = validate_repository(ROOT)
    if failures:
        for failure in failures:
            print(f"toolchain ownership check failed: {failure}", file=sys.stderr)
        return 1
    print("toolchain ownership: pass")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
