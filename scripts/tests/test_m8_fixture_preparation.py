"""Explicit fixture publication and rejected Cargo evidence without a compiler."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import m8_inputs
spec = importlib.util.spec_from_file_location('m8_prepare', Path(__file__).resolve().parents[1] / 'prepare-m8-fixture.py')
module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)


def artifact(binary):
    return {'reason': 'compiler-artifact', 'target': {'name': m8_inputs.FIXTURE, 'kind': ['bin']},
            'profile': {'opt_level': '3', 'debuginfo': 0, 'debug_assertions': False,
                        'overflow_checks': False, 'test': False}, 'executable': str(binary)}


def encoded(events):
    return '\n'.join(map(json.dumps, events)).encode()


class PreparationTests(unittest.TestCase):
    def test_publication_binds_selected_cargo_artifact_and_reuses_verified_receipt(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); binary = root / 'target/release' / m8_inputs.FIXTURE
            binary.parent.mkdir(parents=True); binary.write_bytes(b'native executable')
            product = {'identity_sha256': 'a' * 64, 'identity': {
                'source': {'commit': 'b' * 40, 'tree_sha256': 'c' * 64},
                'target': 'aarch64-apple-darwin', 'toolchains': {'rust': 'compiler'}}}
            result = subprocess.CompletedProcess([], 0, encoded([artifact(binary), {'reason': 'build-finished', 'success': True}]), b'compiler diagnostics')
            with patch.object(module.native_candidate, 'verify', return_value=product), \
                    patch.object(module.native_candidate, 'build_identity', return_value=product['identity']) as identity, \
                    patch.object(module, 'run_sample', return_value=result) as build:
                receipt = module.prepare(root / 'candidate', root / 'published', root / 'target')
                self.assertEqual((receipt.parent / m8_inputs.FIXTURE).read_bytes(), binary.read_bytes())
                self.assertEqual(json.loads(receipt.read_text())['candidate_identity'], 'a' * 64)
                self.assertEqual(module.prepare(root / 'candidate', root / 'published', root / 'target'), receipt)
                self.assertEqual(build.call_count, 1)
                self.assertEqual(build.call_args.kwargs['timeout'], 7200)
                self.assertEqual(build.call_args.kwargs['output_limit'], 64 * 1024 * 1024)
                self.assertEqual(next(root.glob('.m8-build-*/stderr.log')).read_bytes(), result.stderr)
                identity.return_value = {'source': 'different'}
                with self.assertRaises(ValueError):
                    module.prepare(root / 'candidate', root / 'published', root / 'target')
                identity.return_value = product['identity']
                executable = receipt.parent / m8_inputs.FIXTURE
                executable.chmod(0o700); executable.write_bytes(b'changed'); executable.chmod(0o500)
                with self.assertRaises(ValueError):
                    module.prepare(root / 'candidate', root / 'published', root / 'target')
                self.assertEqual(build.call_count, 1, 'verification must not repair a changed published bundle')

    def test_wrong_build_tuple_refuses_before_compilation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with patch.object(module.native_candidate, 'verify', return_value={'identity': {'source': 'approved'}}), \
                    patch.object(module.native_candidate, 'build_identity', return_value={'source': 'changed'}), \
                    patch.object(module, 'run_sample') as build:
                with self.assertRaises(ValueError):
                    module.prepare(root, root / 'new', root / 'target')
                build.assert_not_called()

    def test_malformed_live_compiler_output_times_out_and_is_reaped_before_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); pid_file = root / 'compiler.pid'
            product = {'identity': {'target': 'aarch64-apple-darwin'}}
            owned_run = module.run_sample
            def compiler(_command, **options):
                self.assertEqual(options['timeout'], 7200)
                options['timeout'] = .2
                body = (f"import os,pathlib,time; pathlib.Path({str(pid_file)!r}).write_text(str(os.getpid())); "
                        "print('malformed cargo json',flush=True); time.sleep(30)")
                return owned_run([sys.executable, '-c', body], **options)
            with patch.object(module.native_candidate, 'verify', return_value=product), \
                    patch.object(module.native_candidate, 'build_identity', return_value=product['identity']), \
                    patch.object(module, 'run_sample', side_effect=compiler), self.assertRaises(TimeoutError):
                module.prepare(root / 'candidate', root / 'published', root / 'target')
            self.assertFalse((root / 'published').exists())
            self.assertIn(b'malformed cargo json', next(root.glob('.m8-build-*/drain.log')).read_bytes())
            with self.assertRaises(ProcessLookupError):
                os.kill(int(pid_file.read_text()), 0)

    def test_rejects_missing_failed_duplicate_or_instrumented_compiler_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory); binary = target / 'release' / m8_inputs.FIXTURE
            binary.parent.mkdir(); binary.write_bytes(b'image')
            event = artifact(binary); finished = {'reason': 'build-finished', 'success': True}
            for events in ([event], [event, {'reason': 'build-finished', 'success': False}],
                           [event, event, finished], [event, finished, finished],
                           [{**event, 'profile': {**event['profile'], 'test': True}}, finished],
                           [{**event, 'profile': {**event['profile'], 'debuginfo': 2}}, finished]):
                with self.subTest(events=events), self.assertRaises(ValueError):
                    module.cargo_artifact(0, encoded(events), target, 'aarch64-apple-darwin')
            with self.assertRaises(ValueError):
                module.cargo_artifact(1, encoded([event, finished]), target, 'aarch64-apple-darwin')
            (target / 'other').mkdir()
            with self.assertRaises(ValueError):
                module.cargo_artifact(0, encoded([event, finished]), target / 'other', 'aarch64-apple-darwin')
            with self.assertRaises(ValueError):
                module.cargo_artifact(0, b'invalid json', target, 'aarch64-apple-darwin')


if __name__ == '__main__': unittest.main()
