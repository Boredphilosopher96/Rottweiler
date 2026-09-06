import unittest
import tempfile
from pathlib import Path

import native_profile


class NativeProfileTests(unittest.TestCase):
    def test_owned_flags_follow_existing_encoded_flags_without_retokenizing_them(self):
        result = native_profile.environment("x86_64-unknown-linux-gnu", {
            "CARGO_ENCODED_RUSTFLAGS": "--cfg\x1fname=\"two words\"",
            "RUSTFLAGS": "must-not-be-used",
        })
        flags = result["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")
        self.assertEqual(flags[:2], ["--cfg", 'name="two words"'])
        expected = native_profile.settings("x86_64-unknown-linux-gnu")["rustflags"]
        script = str(Path(native_profile.__file__).resolve().parent.parent / native_profile.UNWIND_SCRIPT)
        self.assertEqual(flags[2:], [flag.replace(native_profile.UNWIND_SCRIPT_TOKEN, script) for flag in expected])
        self.assertNotIn(native_profile.UNWIND_SCRIPT_TOKEN, result["CARGO_ENCODED_RUSTFLAGS"])

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

    def test_linker_script_bytes_are_part_of_the_portable_profile_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)
            script = repo / native_profile.UNWIND_SCRIPT
            script.parent.mkdir()
            script.write_text("SECTIONS { /DISCARD/ : { *(.eh_frame) } } INSERT AFTER .text;")
            first = native_profile.settings("x86_64-unknown-linux-gnu", repo)
            script.write_text(script.read_text() + "\n/* changed */\n")
            second = native_profile.settings("x86_64-unknown-linux-gnu", repo)
            self.assertNotEqual(first["linker_script"], second["linker_script"])
            self.assertEqual(first["rustflags"], second["rustflags"])
            self.assertNotIn(str(repo), str(first))

    def test_missing_linker_script_is_not_silently_ignored(self):
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaises(FileNotFoundError):
                native_profile.settings("x86_64-unknown-linux-gnu", Path(temporary))

    def test_optimized_harnesses_keep_unwind_tables_and_record_distinct_flags(self):
        for target in ["x86_64-unknown-linux-gnu", "aarch64-apple-darwin"]:
            profile = native_profile.verification_settings(target)
            self.assertEqual(profile["opt_level"], native_profile.settings(target)["opt_level"])
            flags = native_profile.verification_environment(target, {})["CARGO_ENCODED_RUSTFLAGS"]
            self.assertEqual(profile["rustflags"], native_profile.settings(target)["rustflags"])
            self.assertNotIn("--no-eh-frame-hdr", flags)
            self.assertNotIn("link-arg=-T", flags)
            self.assertNotIn("linker_script", profile)

    def test_final_policy_is_only_passed_to_the_selected_native_binary(self):
        command = native_profile.product_command("x86_64-unknown-linux-gnu", ["build", "--release", "-p", "rw-cli", "--bin", "rw"])
        self.assertEqual(command[:2], ["cargo", "rustc"])
        self.assertIn("link-arg=-Wl,--no-eh-frame-hdr", command[command.index("--") + 1:])
        for arguments in [["test", "--bin", "rw"], ["build", "--tests", "--bin", "rw"], ["build", "-p", "rw-cli"]]:
            with self.assertRaisesRegex(ValueError, "explicit --bin"):
                native_profile.product_command("x86_64-unknown-linux-gnu", arguments)
