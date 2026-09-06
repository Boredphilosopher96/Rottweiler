#!/usr/bin/env python3
"""Enforce the 1,500-line ceiling for repository-owned source and tests."""
from __future__ import annotations

import hashlib
from pathlib import Path
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
MAX_LINES = 1500
SOURCE_SUFFIXES = {'.rs', '.ts', '.tsx', '.js', '.jsx', '.mjs', '.cjs', '.mts', '.py', '.sh', '.zig', '.wit'}
# Immutable third-party acceptance data, not maintained product code.
VENDORED = {
    'crates/rw-core/tests/fixtures/init/python/bottle.py': '88955d5807e93a2da4b0f665c99b402dcccf8fd6aaa9c357ad25d20a55022707',
}


def line_count(data: bytes) -> int:
    return data.count(b'\n') + int(bool(data) and not data.endswith(b'\n'))


def generated_outputs(root: Path) -> dict[str, bytes]:
    manifest = tomllib.loads((root / 'architecture/ownership.toml').read_text())
    return {
        output: generator['marker'].encode()
        for generator in manifest.get('generator', [])
        for output in generator.get('outputs', [])
        if generator.get('command') and generator.get('marker')
    }


def violations(root: Path, paths: list[str], generated: dict[str, bytes]) -> list[str]:
    failures = []
    for name in sorted(set(paths)):
        path = root / name
        if path.suffix not in SOURCE_SUFFIXES or not path.is_file():
            continue
        data = path.read_bytes()
        if name in VENDORED:
            if hashlib.sha256(data).hexdigest() != VENDORED[name]:
                failures.append(f'{name}: vendored fixture changed; verify its upstream provenance')
            continue
        if name in generated:
            if generated[name] not in data[:4096]:
                failures.append(f'{name}: registered generated output is missing its ownership marker')
            continue
        count = line_count(data)
        if count > MAX_LINES:
            failures.append(f'{name}: {count} lines exceeds {MAX_LINES}; split by responsibility')
    return failures


def main() -> int:
    result = subprocess.run(
        ['git', 'ls-files', '-z', '--cached', '--others', '--exclude-standard'],
        cwd=ROOT, check=True, stdout=subprocess.PIPE,
    )
    paths = [path for path in result.stdout.decode().split('\0') if path]
    failures = violations(ROOT, paths, generated_outputs(ROOT))
    if failures:
        print('\n'.join(failures), file=sys.stderr)
        return 1
    print(f'Source size passed: every handwritten source and test file is <= {MAX_LINES} lines.')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
