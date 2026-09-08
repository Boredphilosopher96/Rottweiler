"""Explicit fixture build publication and reuse, without invoking a compiler."""
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import m8_inputs
spec = importlib.util.spec_from_file_location('m8_prepare', Path(__file__).resolve().parents[1] / 'prepare-m8-fixture.py')
module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)


class PreparationTests(unittest.TestCase):
    def test_publication_binds_selected_cargo_artifact_and_reuses_verified_receipt(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); binary = root / 'built'; binary.write_bytes(b'native executable')
            product = {'identity_sha256': 'a' * 64, 'identity': {
                'source': {'commit': 'b' * 40, 'tree_sha256': 'c' * 64},
                'target': 'aarch64-apple-darwin', 'toolchains': {'rust': 'compiler'}}}
            event = {'reason': 'compiler-artifact', 'target': {'name': m8_inputs.FIXTURE, 'kind': ['bin']},
                     'executable': str(binary)}
            class Build:
                stdout = [json.dumps(event) + '\n']
                def __enter__(self): return self
                def __exit__(self, *_): pass
                def wait(self): return 0
            with patch.object(module.native_candidate, 'verify', return_value=product), \
                    patch.object(module.native_candidate, 'build_identity', return_value=product['identity']), \
                    patch.object(module.subprocess, 'Popen', return_value=Build()) as build:
                receipt = module.prepare(root / 'candidate', root / 'published', root / 'target')
                self.assertEqual((receipt.parent / m8_inputs.FIXTURE).read_bytes(), binary.read_bytes())
                self.assertEqual(json.loads(receipt.read_text())['candidate_identity'], 'a' * 64)
                self.assertEqual(module.prepare(root / 'candidate', root / 'published', root / 'target'), receipt)
                self.assertEqual(build.call_count, 1)
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
                    patch.object(module.subprocess, 'Popen') as build:
                with self.assertRaises(ValueError):
                    module.prepare(root, root / 'new', root / 'target')
                build.assert_not_called()


if __name__ == '__main__': unittest.main()
