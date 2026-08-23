#!/usr/bin/env python3
"""Check first-party Markdown links and high-risk current-product claims."""

from __future__ import annotations

import re
import subprocess
from pathlib import Path
from urllib.parse import unquote

ROOT = Path(__file__).resolve().parents[1]
HISTORICAL_PREFIXES = (
    "crates/rw-context/spec/toon/",
    "crates/rw-core/tests/fixtures/",
    "docs/gaps/",
    "docs/reviews/",
)
LINK = re.compile(r"(?<!!)\[[^\]]*\]\(([^)]+)\)")
FENCE = re.compile(r"```.*?```", re.DOTALL)


def markdown_paths() -> list[Path]:
    result = subprocess.run(
        ["git", "-C", str(ROOT), "ls-files", "--cached", "--others", "--exclude-standard", "*.md", "*.mdx"],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    )
    return [ROOT / value for value in sorted(set(result.stdout.splitlines())) if value]


def is_historical(path: Path) -> bool:
    relative = path.relative_to(ROOT).as_posix()
    return relative.startswith(HISTORICAL_PREFIXES)


def check_links(path: Path, source: str) -> list[str]:
    failures: list[str] = []
    without_code = FENCE.sub("", source)
    for match in LINK.finditer(without_code):
        value = match.group(1).strip().split(maxsplit=1)[0].strip("<>")
        if not value or value.startswith(("#", "http://", "https://", "mailto:")):
            continue
        value = unquote(value.split("#", 1)[0].split("?", 1)[0])
        if not value or value.startswith("/Rottweiler/") or any(token in value for token in ("{", "}", "<", ">")):
            continue
        target = (path.parent / value).resolve()
        try:
            target.relative_to(ROOT)
        except ValueError:
            failures.append(f"{path.relative_to(ROOT)} links outside the repository: {match.group(1)}")
            continue
        if not target.exists():
            failures.append(f"{path.relative_to(ROOT)} has a broken link: {match.group(1)}")
    return failures


def main() -> int:
    failures: list[str] = []
    paths = markdown_paths()
    active = [path for path in paths if not is_historical(path)]
    for path in active:
        source = path.read_text(encoding="utf-8")
        failures.extend(check_links(path, source))

    current_sources = "\n".join(path.read_text(encoding="utf-8") for path in active)
    forbidden = {
        "rw --line": "deleted line-client flag",
        "rw sessions recent": "deleted session alias",
        "docs/spec/toon/": "obsolete TOON path",
        "no stable release is supported": "obsolete pre-release claim",
    }
    for needle, reason in forbidden.items():
        if needle in current_sources:
            failures.append(f"current documentation contains {reason}: {needle}")

    stale_package_refs = [
        path.relative_to(ROOT).as_posix()
        for path in active
        if "packages/plugin-docs" in path.read_text(encoding="utf-8")
        and path.relative_to(ROOT).as_posix() != "docs/design/documentation-site.md"
    ]
    if stale_package_refs:
        failures.append(f"deleted plugin-docs package is referenced by: {', '.join(stale_package_refs)}")

    if failures:
        for failure in failures:
            print(f"documentation check failed: {failure}")
        return 1
    print(f"documentation: pass ({len(active)} current, {len(paths) - len(active)} historical/vendored/fixture Markdown files)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
