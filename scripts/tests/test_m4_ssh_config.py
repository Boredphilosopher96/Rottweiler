"""An explicit SSH profile must reach preflight without user configuration edits."""
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

PATH = Path(__file__).resolve().parents[2] / "crates/rw-cli/tests/m4_release_gate.py"
SPEC = importlib.util.spec_from_file_location("m4_ssh_profile", PATH)
M4 = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M4
SPEC.loader.exec_module(M4)


class M4SshProfileTests(unittest.TestCase):
    def test_explicit_profile_is_one_exact_argument_and_default_omits_it(self):
        with tempfile.TemporaryDirectory() as directory:
            profile = Path(directory) / "isolated ssh profile"
            profile.write_text("Host fixture\n HostName localhost\n")
            with patch.object(M4, "run_sample", return_value=subprocess.CompletedProcess([], 0, b"", b"")) as run:
                M4.ssh_preflight("fixture", profile)
                self.assertEqual(run.call_args.args[0], ["/usr/bin/ssh", "-F", str(profile), "-T", "-o", "BatchMode=yes", "--", "fixture", "true"])
                M4.ssh_preflight("fixture", None)
                self.assertEqual(run.call_args.args[0], ["/usr/bin/ssh", "-T", "-o", "BatchMode=yes", "--", "fixture", "true"])

    def test_invalid_profile_is_rejected_before_any_process_launch(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fifo = root / "fifo"
            os.mkfifo(fifo)
            with patch.object(M4, "run_sample") as run:
                for path in (Path("relative"), root / "missing", root, fifo):
                    with self.subTest(path=path), self.assertRaises((ValueError, OSError)):
                        M4.ssh_preflight("fixture", path)
                run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
