"""Native shell acceptance follows rendered cells across incremental frames."""
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from m4_gate_support import PtyProcess, SHELL_EXIT_MARKER, SHELL_INTERRUPT_MARKER, read_until, read_until_all
from m4_terminal_screen import TerminalScreen


class ShellRenderObserverTests(unittest.TestCase):
    def test_partial_status_repaint_requires_retained_header_and_interrupt_receipt(self):
        process, screen = PtyProcess(123, 1), TerminalScreen(100, 30)
        initial = "\x1b[2;1HTerminal · running\x1b[3;1HREADY".encode()
        update = b"\x1b[2;12Hexit 23\x1b[K"
        receipt = b"\x1b[4;1H" + SHELL_INTERRUPT_MARKER.encode()
        with patch("m4_gate_support.select.select", return_value=([1], [], [])), \
                patch("m4_gate_support.os.read", side_effect=[initial, update, receipt]):
            read_until(process, b"READY", screen=screen)
            self.assertNotIn(SHELL_EXIT_MARKER, screen.text)
            captured = read_until_all(process, (SHELL_INTERRUPT_MARKER.encode(),), screen=screen,
                                      rendered_markers=(SHELL_EXIT_MARKER,))
        self.assertNotIn(SHELL_EXIT_MARKER.encode(), initial + captured,
                         "old contiguous-byte assertion must fail for this valid renderer update")
        self.assertIn(SHELL_EXIT_MARKER, screen.text)
        self.assertIn(SHELL_INTERRUPT_MARKER.encode(), captured)

    def test_raw_hidden_status_never_substitutes_for_visible_completed_state(self):
        screen = TerminalScreen(100, 30)
        screen.feed("Terminal · running".encode())
        hidden = (SHELL_INTERRUPT_MARKER + "\x1b]0;" + SHELL_EXIT_MARKER + "\x07").encode()
        with patch("m4_gate_support.select.select", return_value=([1], [], [])), \
                patch("m4_gate_support.os.read", side_effect=[hidden, b""]), \
                patch.object(PtyProcess, "exit_status", return_value=None):
            with self.assertRaisesRegex(RuntimeError, "rendered_markers"):
                read_until_all(PtyProcess(123, 1), (SHELL_INTERRUPT_MARKER.encode(),), screen=screen,
                               rendered_markers=(SHELL_EXIT_MARKER,))

    def test_rendered_marker_requires_explicit_screen_owner(self):
        with self.assertRaises(ValueError):
            read_until_all(PtyProcess(123, 1), (), rendered_markers=(SHELL_EXIT_MARKER,))


if __name__ == "__main__":
    unittest.main()
