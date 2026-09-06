from __future__ import annotations

import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]


def load(name: str):
    spec = importlib.util.spec_from_file_location(name, ROOT / 'scripts' / f'{name}.py')
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


CLEAN = load('clean-build-artifacts')
SIZE = load('check-source-size')


class CleanupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        subprocess.run(['git', 'init', '-q', str(self.root)], check=True)
        self.target = self.root / 'target'
        self.target.mkdir()
        (self.target / 'CACHEDIR.TAG').write_text('cargo cache')
        self.artifact = CLEAN.Artifact(self.target, 'cargo', self.root)

    def test_accepts_cargo_output_and_parent_alias(self):
        CLEAN.validate(self.artifact, [self.root])
        alias = self.root / 'alias'
        alias.symlink_to(self.root, target_is_directory=True)
        self.assertEqual(CLEAN.canonical_path(alias / 'target'), self.target)

    def test_rejects_symlink_target(self):
        alias = self.root / 'aliased-target'
        alias.symlink_to(self.target, target_is_directory=True)
        artifact = CLEAN.Artifact(CLEAN.canonical_path(alias), 'cargo', self.root)
        with self.assertRaisesRegex(ValueError, 'regular artifact'):
            CLEAN.validate(artifact, [self.root])

    def test_rejects_workspace_and_ancestor(self):
        for path in [self.root, self.root.parent]:
            with self.subTest(path=path), self.assertRaisesRegex(ValueError, 'workspace'):
                CLEAN.validate(CLEAN.Artifact(path, 'cargo', self.root), [self.root])

    def test_rejects_tracked_files(self):
        subprocess.run(['git', 'add', 'target/CACHEDIR.TAG'], cwd=self.root, check=True)
        with self.assertRaisesRegex(ValueError, 'tracked files'):
            CLEAN.validate(self.artifact, [self.root])

    def test_rejects_nested_project_and_unmarked_target(self):
        (self.target / 'Cargo.toml').write_text('[package]')
        with self.assertRaisesRegex(ValueError, 'project'):
            CLEAN.validate(self.artifact, [self.root])
        (self.target / 'Cargo.toml').unlink()
        (self.target / 'CACHEDIR.TAG').unlink()
        with self.assertRaisesRegex(ValueError, 'marker'):
            CLEAN.validate(self.artifact, [self.root])

    def test_default_scope_preserves_dependencies_and_evidence(self):
        paths = {a.path for a in CLEAN.candidates([self.root], ['packages/tui'], False)}
        self.assertEqual(paths, {self.root / p for p in
                               ['target', 'fuzz/target', 'dist', 'packages/tui/dist']})
        expanded = {a.path for a in CLEAN.candidates([self.root], ['packages/tui'], True)}
        self.assertEqual(expanded - paths, {self.root / 'packages/tui/node_modules'})

    def test_explicit_target_cannot_contain_another_registered_worktree(self):
        other = self.target / 'other-worktree'
        other.mkdir()
        (self.root / 'contracts').mkdir()
        (self.root / 'contracts/package-inventory.json').write_text('{"packages": []}')
        with patch.object(CLEAN, 'ROOT', self.root), \
                patch.object(CLEAN, 'worktrees', return_value=[self.root, other]), \
                patch.object(sys, 'argv', ['clean', '--target-dir', str(self.target)]), \
                self.assertRaisesRegex(ValueError, 'workspace'):
            CLEAN.main()

    def test_explicit_target_preview_does_not_clean_other_targets(self):
        with patch.object(sys, 'argv', ['clean', '--target-dir', str(self.target)]), \
                patch.object(CLEAN, 'candidates', side_effect=AssertionError('wrong scope')), \
                patch.object(CLEAN.shutil, 'rmtree', side_effect=AssertionError('preview deleted')):
            self.assertEqual(CLEAN.main(), 0)
        self.assertTrue((self.target / 'CACHEDIR.TAG').exists())


class SourceSizeTests(unittest.TestCase):
    def test_counts_physical_lines(self):
        for data, count in [(b'', 0), (b'x', 1), (b'x\n', 1), (b'\r\n' * 1500, 1500)]:
            self.assertEqual(SIZE.line_count(data), count)

    def test_limit_and_generated_marker_spoof(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / 'module.rs'
            path.write_bytes(b'// generated\n' * 1500)
            self.assertEqual(SIZE.violations(root, ['module.rs'], {}), [])
            path.write_bytes(b'// generated\n' * 1501)
            self.assertIn('1501', SIZE.violations(root, ['module.rs'], {})[0])
            self.assertEqual(SIZE.violations(root, ['module.rs'], {'module.rs': b'// generated'}), [])
            self.assertIn('missing', SIZE.violations(root, ['module.rs'], {'module.rs': b'owner marker'})[0])

    def test_vendor_modification_fails_even_below_cap(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            name = next(iter(SIZE.VENDORED))
            path = root / name
            path.parent.mkdir(parents=True)
            path.write_text('changed\n')
            self.assertIn('vendored fixture changed', SIZE.violations(root, [name], {})[0])

    def test_checks_tracked_and_untracked_but_not_ignored_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(['git', 'init', '-q', str(root)], check=True)
            (root / '.gitignore').write_text('ignored.rs\n')
            for name in ['tracked.rs', 'untracked.py', 'ignored.rs']:
                (root / name).write_bytes(b'\n' * 1501)
            subprocess.run(['git', 'add', 'tracked.rs'], cwd=root, check=True)
            with patch.object(SIZE, 'ROOT', root), patch.object(SIZE, 'generated_outputs', return_value={}), \
                    patch('builtins.print') as output:
                self.assertEqual(SIZE.main(), 1)
            message = output.call_args.args[0]
            self.assertIn('tracked.rs', message)
            self.assertIn('untracked.py', message)
            self.assertNotIn('ignored.rs', message)


if __name__ == '__main__':
    unittest.main()
