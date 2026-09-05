#!/usr/bin/env python3
"""Preview or remove disposable build output without touching project evidence."""
from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]


@dataclass(frozen=True)
class Artifact:
    path: Path
    kind: str
    workspace: Path


def canonical_path(path: Path) -> Path:
    """Resolve parent aliases (including macOS /tmp), but preserve a symlink leaf."""
    return path.absolute().parent.resolve() / path.name


def worktrees(root: Path) -> list[Path]:
    result = subprocess.run(['git', 'worktree', 'list', '--porcelain', '-z'],
                            cwd=root, check=True, stdout=subprocess.PIPE)
    return [Path(field.removeprefix('worktree '))
            for field in result.stdout.decode().split('\0') if field.startswith('worktree ')]


def candidates(roots: list[Path], packages: list[str], dependencies: bool) -> list[Artifact]:
    result = []
    for root in roots:
        result.extend(Artifact(root / path, 'cargo', root) for path in ('target', 'fuzz/target'))
        result.append(Artifact(root / 'dist', 'directory', root))
        for package in packages:
            result.append(Artifact(root / package / 'dist', 'directory', root))
            if dependencies:
                result.append(Artifact(root / package / 'node_modules', 'directory', root))
    return result


def validate(artifact: Artifact, roots: list[Path]) -> None:
    path = artifact.path
    if not path.is_absolute() or path.is_symlink() or not path.is_dir():
        raise ValueError(f'not a regular artifact directory: {path}')
    resolved = path.resolve()
    if resolved != canonical_path(path) or any(root.resolve() == resolved or root.resolve().is_relative_to(resolved)
                               for root in roots):
        raise ValueError(f'artifact path aliases or contains a workspace: {path}')
    if (path / '.git').exists() or (path / 'Cargo.toml').exists():
        raise ValueError(f'artifact directory contains a project: {path}')
    for root in roots:
        if not path.is_relative_to(root):
            continue
        tracked = subprocess.run(['git', 'ls-files', '-z', '--', str(path.relative_to(root))],
                                 cwd=root, check=True, stdout=subprocess.PIPE).stdout
        if tracked:
            raise ValueError(f'artifact directory contains tracked files: {path}')
    if artifact.kind == 'cargo' and not any((path / marker).is_file() for marker in
                                            ('.rustc_info.json', 'CACHEDIR.TAG', 'debug/.cargo-lock', 'release/.cargo-lock')):
        raise ValueError(f'no Cargo artifact marker found: {path}')


def disk_bytes(path: Path) -> int:
    total = 0
    for directory, dirs, files in os.walk(path, followlinks=False):
        for name in dirs + files:
            stat = (Path(directory) / name).lstat()
            total += getattr(stat, 'st_blocks', 0) * 512 or stat.st_size
    return total


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--apply', action='store_true', help='remove the displayed build output')
    scope = parser.add_mutually_exclusive_group()
    scope.add_argument('--worktrees', action='store_true', help='include registered repository worktrees')
    parser.add_argument('--dependencies', action='store_true', help='also remove package node_modules')
    scope.add_argument('--target-dir', action='append', default=[], type=Path,
                       help='clean ONLY these exact Cargo targets; may repeat')
    args = parser.parse_args()
    roots = [root.resolve() for root in worktrees(ROOT)] if args.worktrees else [ROOT]
    packages = [entry['directory'] for entry in json.loads(
        (ROOT / 'contracts/package-inventory.json').read_text())['packages']]
    artifacts = ([Artifact(canonical_path(path), 'cargo', ROOT) for path in args.target_dir]
                 if args.target_dir else candidates(roots, packages, args.dependencies))
    selected = {}
    for artifact in artifacts:
        if not artifact.path.exists() and not artifact.path.is_symlink():
            continue
        validate(artifact, roots)
        selected[artifact.path] = artifact
    total = 0
    for artifact in selected.values():
        size = disk_bytes(artifact.path)
        total += size
        print(f'{size / (1024 ** 3):8.2f} GiB  {artifact.path}', flush=True)
    print(f'{total / (1024 ** 3):.2f} GiB estimated disposable output.', flush=True)
    if not args.apply:
        print('Preview only. Stop builds/tests, then repeat with --apply to clean.')
        return 0
    for artifact in selected.values():
        validate(artifact, roots)
        print(f'Cleaning {artifact.path}', flush=True)
        if artifact.kind == 'cargo':
            subprocess.run(['cargo', 'clean', '--target-dir', str(artifact.path)],
                           cwd=artifact.workspace, check=True)
        else:
            shutil.rmtree(artifact.path)
    return 0


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        raise SystemExit(str(error)) from error
