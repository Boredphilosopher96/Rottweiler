import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import tui_tests


class TuiRunnerTests(unittest.TestCase):
    def test_each_performance_vm_inherits_nonblocking_ack_and_preserves_arguments(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bun = root / "bun"
            bun.write_text("#!" + sys.executable + "\n" +
                "import json,os,sys\n"
                "assert not os.get_blocking(int(os.environ['RW_PERF_SETTLEMENT_FD']))\n"
                "from perf_process_owner import SCOPE\n"
                "with open(os.environ['TUI_CALLS'],'a') as log: log.write(json.dumps(sys.argv[1:])+'\\n')\n")
            bun.chmod(0o700)
            evidence = root / "evidence.json"
            result = subprocess.run([
                sys.executable, str(SCRIPTS / "ci_evidence.py"), "--delegated", "--gate", "tui",
                "--output", str(evidence), "--", sys.executable, str(SCRIPTS / "tui_tests.py"), "test:perf",
            ], env=dict(os.environ, PATH=str(root) + ":" + os.environ["PATH"],
                        PYTHONPATH=str(SCRIPTS), TUI_CALLS=str(root / "calls")), capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            self.assertNotIn("cleanup_error", json.loads(evidence.read_text()))
            self.assertEqual([json.loads(line) for line in (root / "calls").read_text().splitlines()],
                             [command[1:] for command in tui_tests.commands("test:perf")])

    def test_package_scripts_have_one_authoritative_direct_vm_runner(self):
        package = json.loads((SCRIPTS.parent / "packages/tui/package.json").read_text())
        for mode in ("test", "test:perf"):
            self.assertEqual(package["scripts"][mode], f"python3 ../../scripts/tui_tests.py {mode}")
        self.assertEqual(tui_tests.commands("test"),
                         [["bun", "test", "--max-concurrency=1", "--path-ignore-patterns=test/perf/**"]])
