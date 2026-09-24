from __future__ import annotations

import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location("m4_terminal_screen", Path(__file__).resolve().parents[1] / "m4_terminal_screen.py")
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
TerminalScreen = MODULE.TerminalScreen


class TerminalScreenTests(unittest.TestCase):
    def test_partial_redraw_reconstructs_marker_without_contiguous_bytes(self):
        screen = TerminalScreen(30, 4)
        screen.feed(b"\x1b[2J\x1b[2;10He")
        screen.feed(b"\x1b[2;4HProvid\x1b[2;11Hr name")
        self.assertIn("Provider name", screen.text)
        self.assertNotIn("\x1b", screen.text)

    def test_split_utf8_csi_and_combining_wide_cells(self):
        stream = "\x1b[2;3H界e\u0301\x1b[2;7H!\x1b[3;1H✅ ready".encode()
        whole, split = TerminalScreen(20, 4), TerminalScreen(20, 4)
        whole.feed(stream)
        for byte in stream: split.feed(bytes([byte]))
        self.assertEqual(whole.text, split.text)
        self.assertEqual(split.text.splitlines()[1], "  界é !")
        split.feed(b"\x1b[2;4HX")  # overwrite the second cell of the wide glyph
        self.assertEqual(split.text.splitlines()[1], "   Xé !")

    def test_absolute_relative_cursor_and_save_restore(self):
        screen = TerminalScreen(20, 4)
        screen.feed(b"\x1b[2;5HAB\x1b[s\x1b[2D!\x1b[uC\x1b[1A\x1b[2DX\x1b[3d\x1b[2GY")
        self.assertEqual(screen.text.splitlines()[:3], ["     X", "    !BC", " Y"])

    def test_erase_line_screen_and_characters(self):
        screen = TerminalScreen(8, 3)
        screen.feed(b"abcdefgh\r\nABCDEFGH\r\n12345678\x1b[2;3H\x1b[K")
        self.assertEqual(screen.text, "abcdefgh\nAB\n12345678")
        screen.feed(b"\x1b[1;4H\x1b[1J")
        self.assertEqual(screen.text.splitlines()[0], "    efgh")
        screen.feed(b"\x1b[3;3H\x1b[2X")
        self.assertEqual(screen.text.splitlines()[2], "12  5678")
        screen.feed(b"\x1b[2J")
        self.assertEqual(screen.text, "\n\n")

    def test_styles_osc_dcs_and_unknown_control_sequences_never_become_text(self):
        screen = TerminalScreen(30, 3)
        for chunk in [b"\x1b[38;2;1;2;3mvisible\x1b[0m", b"\x1b]8;;https://secret", b"\x1b\\link\x1b]8;;\x07", b"\x1bPpayload\x1b", b"\\\x1b[?2026h!\x1b[?2026l"]:
            screen.feed(chunk)
        self.assertEqual(screen.text, "visiblelink!\n\n")

    def test_alternate_screen_restores_primary_and_cursor(self):
        screen = TerminalScreen(20, 3)
        screen.feed(b"shell\x1b[?1049hUI")
        self.assertEqual(screen.text, "UI\n\n")
        screen.feed(b"\x1b[?1049l>")
        self.assertEqual(screen.text, "shell>\n\n")

    def test_wrap_scroll_and_scroll_region(self):
        screen = TerminalScreen(4, 4)
        screen.feed(b"aaaabbbbccccdddd")
        self.assertEqual(screen.text, "aaaa\nbbbb\ncccc\ndddd")
        screen.feed(b"E")
        self.assertEqual(screen.text, "bbbb\ncccc\ndddd\nE")
        screen.feed(b"\x1b[2;3r\x1b[3;1H\n")
        self.assertEqual(screen.text, "bbbb\ndddd\n\nE")

    def test_insert_delete_and_line_operations(self):
        screen = TerminalScreen(8, 4)
        screen.feed(b"abcdef\x1b[1;3H\x1b[2@XY\x1b[1P")
        self.assertEqual(screen.text.splitlines()[0], "abXYdef")
        screen.feed(b"\x1b[2;1Hsecond\x1b[3;1Hthird\x1b[2;1H\x1b[L")
        self.assertEqual(screen.text, "abXYdef\n\nsecond\nthird")
        screen.feed(b"\x1b[M")
        self.assertEqual(screen.text, "abXYdef\nsecond\nthird\n")

    def test_disable_wrap_and_control_strings_are_bounded(self):
        screen = TerminalScreen(4, 2)
        screen.feed(b"\x1b[?7habcd\x1b[?7lEF")
        self.assertEqual(screen.text, "abcF\n")
        screen.feed(b"\x1b]" + b"x" * 100000 + b"\x07\r!")
        self.assertEqual(screen.text, "!bcF\n")
        screen.feed(b"\x1b[" + b"1;" * 1000 + b"m\r?")
        self.assertEqual(screen.text, "?bcF\n")

    def test_wrapped_composer_input_matches_with_borders_and_wraps_removed(self):
        import sys
        sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
        from m4_gate_support import unwrapped_screen_text

        rendered = (
            " ╭─ Shell ──────────────╮\n"
            " │ !/opt/python3 /tmp/rw4-x/workspace/m4-shell-   │\n"
            " │ child.py                │\n"
            " ╰──────────────────────╯"
        )
        self.assertNotIn("/tmp/rw4-x/workspace/m4-shell-child.py", rendered)
        self.assertIn("/tmp/rw4-x/workspace/m4-shell-child.py", unwrapped_screen_text(rendered))


if __name__ == "__main__":
    unittest.main()
