import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("artifact_bundle", Path(__file__).resolve().parents[1] / "artifact_bundle.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ArtifactBundleTests(unittest.TestCase):
    def test_native_sidecar_extra_files_and_candidate_identity_are_verified(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "executable").write_bytes(b"executable")
            (root / "native.so").write_bytes(b"native")
            manifest = MODULE.document(root, "a" * 40, "linux-x86_64")
            (root / MODULE.MANIFEST).write_text(json.dumps(manifest))
            MODULE.verify(root, "a" * 40, "linux-x86_64")
            for sha, platform in [("b" * 40, "linux-x86_64"), ("a" * 40, "darwin-arm64")]:
                with self.assertRaises(ValueError):
                    MODULE.verify(root, sha, platform)
            (root / "native.so").write_bytes(b"NATIVE")
            with self.assertRaises(ValueError):
                MODULE.verify(root, "a" * 40, "linux-x86_64")
            (root / "native.so").write_bytes(b"native")
            (root / "extra").write_bytes(b"unexpected")
            with self.assertRaises(ValueError):
                MODULE.verify(root, "a" * 40, "linux-x86_64")
            (root / "extra").unlink()
            (root / "alias").symlink_to(root / "native.so")
            with self.assertRaises(ValueError):
                MODULE.verify(root, "a" * 40, "linux-x86_64")
