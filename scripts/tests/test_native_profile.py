import unittest

import native_profile


class NativeProfileTests(unittest.TestCase):
    def test_owned_flags_follow_existing_encoded_flags_without_retokenizing_them(self):
        result = native_profile.environment("x86_64-unknown-linux-gnu", {
            "CARGO_ENCODED_RUSTFLAGS": "--cfg\x1fname=\"two words\"",
            "RUSTFLAGS": "must-not-be-used",
        })
        flags = result["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")
        self.assertEqual(flags[:2], ["--cfg", 'name="two words"'])
        self.assertEqual(flags[2:], native_profile.settings("x86_64-unknown-linux-gnu")["rustflags"])

    def test_explicit_empty_encoded_flags_override_plain_flags_like_cargo(self):
        result = native_profile.environment("aarch64-apple-darwin", {
            "CARGO_ENCODED_RUSTFLAGS": "", "RUSTFLAGS": "-C force-unwind-tables=no",
        })
        self.assertEqual(result["CARGO_ENCODED_RUSTFLAGS"], "")
        self.assertEqual(result["CARGO_PROFILE_RELEASE_OPT_LEVEL"], "3")

    def test_musl_loader_floor_is_not_inferred_from_gnu_qualification(self):
        flags = native_profile.settings("x86_64-unknown-linux-musl")["rustflags"]
        self.assertEqual(flags, ["-C", "force-unwind-tables=no"])

    def test_unknown_target_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "unsupported"):
            native_profile.settings("wasm32-unknown-unknown")
