"""Strict M8 rejects unbound source/profile and mutated prepared artifacts."""
import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import m8_inputs


class InputTests(unittest.TestCase):
    def test_exact_receipt_and_mutation_denials(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture = root / m8_inputs.FIXTURE
            fixture.write_bytes(b'prepared native fixture'); fixture.chmod(0o500)
            product = {'identity_sha256': 'a' * 64, 'identity': {
                'source': {'commit': 'b' * 40, 'tree_sha256': 'c' * 64},
                'target': 'aarch64-apple-darwin', 'toolchains': {'rust': 'exact compiler'}}}
            body = {'schema_version': 1, 'candidate_identity': product['identity_sha256'],
                    'source': product['identity']['source'], 'target': product['identity']['target'],
                    'rust': 'exact compiler', 'profile': {'opt_level': '3', 'rustflags': []},
                    'fixture': m8_inputs.fixture_identity(fixture)}
            receipt = root / m8_inputs.RECEIPT
            receipt.write_text(json.dumps(body))
            with patch.object(m8_inputs.native_candidate, 'verify', return_value=product):
                self.assertEqual(m8_inputs.verify(root, receipt, root)['prepared'], body)
                for field in body:
                    invalid = copy.deepcopy(body); del invalid[field]
                    receipt.write_text(json.dumps(invalid))
                    with self.subTest(field=field), self.assertRaises(ValueError):
                        m8_inputs.verify(root, receipt, root)
                receipt.write_text(json.dumps(body))
                fixture.chmod(0o700); fixture.write_bytes(b'changed native fixture'); fixture.chmod(0o500)
                with self.assertRaises(ValueError):
                    m8_inputs.verify(root, receipt, root)

    def test_symlink_executable_is_not_prepared_authority(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); target = root / 'target'
            target.write_bytes(b'program'); target.chmod(0o500)
            link = root / m8_inputs.FIXTURE; link.symlink_to(target)
            with self.assertRaises(ValueError):
                m8_inputs.fixture_identity(link)


if __name__ == '__main__':
    unittest.main()
