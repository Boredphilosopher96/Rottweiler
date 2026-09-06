"""Native ownership fixes must be source-qualified and mechanically rebuilt."""
from __future__ import annotations

import copy
import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import opentui_native as native

ROOT = Path(__file__).resolve().parents[2]


class NativeRendererTests(unittest.TestCase):
    def test_every_platform_has_exact_source_toolchain_and_reclaiming_patch(self):
        contract = native.contract(ROOT)
        for platform in contract["zig"]["artifacts"]:
            with patch.object(native, "sdk_identity", return_value=None):
                identity = native.identity(ROOT, platform)
            self.assertEqual(identity["flags"], ["-Doptimize=ReleaseFast", "-Dgpa-safe-stats=false", "-j2"])
            self.assertEqual(len(identity["patches"]), 1)
            self.assertRegex(identity["zig"]["artifact"]["sha256"], r"^[0-9a-f]{64}$")
        source = (ROOT / contract["patches"][0]).read_text()
        self.assertIn("-    const view = editor_view.EditorView.init(globalArena", source)
        self.assertIn("+    const view = editor_view.EditorView.init(globalAllocator", source)
        self.assertIn("-    const pool = gp.initGlobalPool(globalArena)", source)
        self.assertIn("+    const pool = gp.initGlobalPool(globalAllocator)", source)
        self.assertIn("+    const link_pool = link.initGlobalLinkPool(globalAllocator)", source)

    def test_archive_refusal_precedes_publication_and_cache_is_verified(self):
        payload = b"verified source archive"
        artifact = {"url": "https://example.invalid/source", "sha256": hashlib.sha256(payload).hexdigest(), "bytes": len(payload)}
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            for source in (payload + b"overflow", b"short", b"x" * len(payload)):
                with patch.object(native.urllib.request, "urlopen", return_value=io.BytesIO(source)):
                    with self.assertRaises(ValueError):
                        native.download(artifact, directory)
                self.assertEqual(list(directory.iterdir()), [])
            with patch.object(native.urllib.request, "urlopen", return_value=io.BytesIO(payload)):
                admitted = native.download(artifact, directory)
            with patch.object(native.urllib.request, "urlopen", side_effect=AssertionError("no second download")):
                self.assertEqual(native.download(artifact, directory), admitted)
            admitted.write_bytes(b"corrupt")
            with self.assertRaisesRegex(ValueError, "corrupt"):
                native.download(artifact, directory)

    def test_receipt_rejects_missing_identity_modified_binary_and_license(self):
        with patch.object(native, "sdk_identity", return_value=None):
            identity = native.identity(ROOT, "darwin-arm64")
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            library = directory / "libopentui.dylib"
            library.write_bytes(b"native artifact")
            license_path = directory / "opentui-licenses.txt"
            license_path.write_text("license notice")
            proof = directory / "lifetime-probe.json"
            proof.write_text("native lifetime evidence")
            receipt = {"probe_sha256": native.digest(proof), "identity": identity, "library": library.name, "sha256": native.digest(library),
                       "licenses": {license_path.name: native.digest(license_path)}}
            path = directory / native.RECEIPT
            path.write_text(json.dumps(receipt))
            self.assertEqual(native.verify(directory, identity), library)
            missing = copy.deepcopy(receipt)
            del missing["identity"]["zig"]
            path.write_text(json.dumps(missing))
            with self.assertRaises(ValueError):
                native.verify(directory, identity)
            path.write_text(json.dumps(receipt))
            license_path.write_text("changed")
            with self.assertRaisesRegex(ValueError, "license"):
                native.verify(directory, identity)
            license_path.write_text("license notice")
            library.write_bytes(b"changed")
            with self.assertRaises(ValueError):
                native.verify(directory, identity)
