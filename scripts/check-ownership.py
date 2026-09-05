#!/usr/bin/env python3
"""Check the repository's registered data and feature ownership boundaries."""

from __future__ import annotations

from pathlib import Path, PurePosixPath
import re
import sys
import tomllib
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_MANIFEST = ROOT / "architecture" / "ownership.toml"


def _safe_path(value: object) -> str | None:
    if not isinstance(value, str) or not value or "\\" in value:
        return None
    path = PurePosixPath(value)
    if path.is_absolute() or path.as_posix() != value or ".." in path.parts:
        return None
    return value


def _strings(value: object) -> list[str] | None:
    if not isinstance(value, list) or not value:
        return None
    if not all(isinstance(item, str) and item for item in value):
        return None
    return value


def _tables(document: dict[str, Any], key: str, failures: list[str]) -> list[dict[str, Any]]:
    value = document.get(key, [])
    if not isinstance(value, list) or not all(isinstance(item, dict) for item in value):
        failures.append(f"{key!r} must be an array of tables")
        return []
    return value


def _unique_id(
    table: dict[str, Any],
    kind: str,
    seen: set[str],
    failures: list[str],
) -> str | None:
    value = table.get("id")
    if not isinstance(value, str) or not re.fullmatch(r"[a-z][a-z0-9-]*", value):
        failures.append(f"{kind} id must use lower-case letters, digits, and hyphens: {value!r}")
        return None
    if value in seen:
        failures.append(f"duplicate {kind} id: {value}")
    seen.add(value)
    return value


def _load_manifest(manifest: Path) -> tuple[dict[str, Any] | None, list[str]]:
    if not manifest.is_file():
        return None, [f"ownership manifest is missing: {manifest}"]
    try:
        with manifest.open("rb") as file:
            document = tomllib.load(file)
    except (OSError, tomllib.TOMLDecodeError) as error:
        return None, [f"cannot read ownership manifest: {error}"]
    if document.get("version") != 1:
        return None, ["ownership manifest version must be 1"]
    return document, []


def validate_repository(repo_root: Path, manifest: Path) -> list[str]:
    """Return ownership contract failures without changing the repository."""
    document, failures = _load_manifest(manifest)
    if document is None:
        return failures

    owners = _tables(document, "owner", failures)
    generators = _tables(document, "generator", failures)
    shadows = _tables(document, "shadow", failures)

    owner_ids: set[str] = set()
    owner_paths: set[str] = set()
    valid_owner_ids: set[str] = set()
    for owner in owners:
        owner_id = _unique_id(owner, "owner", owner_ids, failures)
        path = _safe_path(owner.get("path"))
        if path is None:
            failures.append(
                f"owner {owner_id or '<invalid>'} path must be a safe repository-relative path"
            )
            continue
        if path in owner_paths:
            failures.append(f"duplicate owner path: {path}")
        owner_paths.add(path)
        owner_file = repo_root / path
        if not owner_file.is_file():
            failures.append(f"owner path does not exist: {path}")
            continue
        symbols = _strings(owner.get("symbols"))
        if symbols is None:
            failures.append(f"owner {owner_id or '<invalid>'} must list at least one symbol")
            continue
        contents = owner_file.read_text(encoding="utf-8")
        for symbol in symbols:
            locator_parts = [part for part in symbol.split("::") if part]
            if not locator_parts or any(part not in contents for part in locator_parts):
                failures.append(f"owner symbol {symbol!r} is missing from {path}")
        if owner_id is not None:
            valid_owner_ids.add(owner_id)

    generator_ids: set[str] = set()
    generator_markers: set[str] = set()
    generated_outputs: set[str] = set()
    for generator in generators:
        generator_id = _unique_id(generator, "generator", generator_ids, failures)
        owner_id = generator.get("owner")
        owner_list = generator.get("owners")
        if owner_id is not None and owner_list is not None:
            failures.append(
                f"generator {generator_id or '<invalid>'} must declare either owner or owners, not both"
            )
            generator_owners: list[str] = []
        elif isinstance(owner_id, str) and owner_id:
            generator_owners = [owner_id]
        else:
            generator_owners = _strings(owner_list) or []
            if not generator_owners:
                failures.append(
                    f"generator {generator_id or '<invalid>'} must reference at least one owner"
                )
        if len(generator_owners) != len(set(generator_owners)):
            failures.append(f"generator {generator_id or '<invalid>'} repeats an owner")
        for referenced_owner in generator_owners:
            if referenced_owner not in valid_owner_ids:
                failures.append(
                    f"generator {generator_id or '<invalid>'} references unknown owner {referenced_owner!r}"
                )
        command = generator.get("command")
        if not isinstance(command, str) or not command.strip():
            failures.append(f"generator {generator_id or '<invalid>'} must declare its check command")
        marker = generator.get("marker")
        if not isinstance(marker, str) or not marker.strip():
            failures.append(f"generator {generator_id or '<invalid>'} must declare a generated marker")
            marker = None
        elif marker in generator_markers:
            failures.append(f"duplicate generated marker: {marker!r}")
        else:
            generator_markers.add(marker)
        outputs = _strings(generator.get("outputs"))
        if outputs is None:
            failures.append(f"generator {generator_id or '<invalid>'} must list generated outputs")
            continue
        for output in outputs:
            safe_output = _safe_path(output)
            if safe_output is None:
                failures.append(
                    f"generator {generator_id or '<invalid>'} output must be a safe repository-relative path: {output!r}"
                )
                continue
            if safe_output in generated_outputs:
                failures.append(f"duplicate generated output: {safe_output}")
            generated_outputs.add(safe_output)
            output_file = repo_root / safe_output
            if not output_file.is_file():
                failures.append(f"generated output does not exist: {safe_output}")
            elif marker is not None and marker not in output_file.read_text(encoding="utf-8"):
                failures.append(
                    f"generated marker for {generator_id or '<invalid>'} is missing from {safe_output}"
                )

    shadow_ids: set[str] = set()
    for shadow in shadows:
        shadow_id = _unique_id(shadow, "shadow", shadow_ids, failures)
        owner_id = shadow.get("owner")
        if owner_id not in valid_owner_ids:
            failures.append(f"shadow {shadow_id or '<invalid>'} references unknown owner {owner_id!r}")
        path = _safe_path(shadow.get("path"))
        if path is None:
            failures.append(
                f"shadow {shadow_id or '<invalid>'} path must be a safe repository-relative path"
            )
            continue
        reason = shadow.get("reason")
        if not isinstance(reason, str) or not reason.strip():
            failures.append(f"shadow {shadow_id or '<invalid>'} must explain the ownership boundary")
        pattern = shadow.get("pattern")
        if not isinstance(pattern, str) or not re.search(r"[A-Za-z_][A-Za-z0-9_]{2,}", pattern):
            failures.append(
                f"shadow {shadow_id or '<invalid>'} pattern must target a named definition"
            )
            continue
        try:
            compiled = re.compile(pattern, re.MULTILINE)
        except re.error as error:
            failures.append(f"shadow {shadow_id or '<invalid>'} has invalid pattern: {error}")
            continue
        shadow_file = repo_root / path
        if not shadow_file.is_file():
            if shadow.get("allow_missing") is not True:
                failures.append(f"shadow path does not exist: {path}")
            continue
        sources = [shadow_file]
        if shadow_file.suffix == ".rs":
            # A Rust file's child modules retain the same ownership constraints.
            module_directory = shadow_file.with_suffix("")
            sources.extend(sorted(module_directory.rglob("*.rs")))
        for source in sources:
            if compiled.search(source.read_text(encoding="utf-8")):
                failures.append(
                    f"forbidden shadow definition {shadow_id or '<invalid>'} exists in "
                    f"{source.relative_to(repo_root)}"
                )

    return failures


def main() -> int:
    failures = validate_repository(ROOT, DEFAULT_MANIFEST)
    if failures:
        for failure in failures:
            print(f"ownership check failed: {failure}", file=sys.stderr)
        return 1
    print("ownership contract: pass")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
