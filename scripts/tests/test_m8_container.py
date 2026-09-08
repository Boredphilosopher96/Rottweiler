"""Daemon identity and failure proofs around the M8 Docker transport."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parents[1]
IDENTITY = "a" * 64


class ContainerOwnershipTests(unittest.TestCase):
    def invoke(self, root, behavior):
        docker = root / "docker"
        docker.write_text("#!" + sys.executable + "\n" +
            "import json,os,sys\n"
            "args=sys.argv[1:]\n"
            "with open(os.environ['DOCKER_LOG'],'a') as log: log.write(json.dumps(args)+'\\n')\n" +
            f"identity={IDENTITY!r}\nbehavior={behavior!r}\n" +
            "if args[0]=='create': print(identity)\n"
            "elif args[0]=='inspect': print('exited 7')\n"
            "elif args[0]=='rm' and behavior=='denied': raise SystemExit(1)\n"
            "elif args[:2]==['container','ls'] and behavior=='retained': print(identity)\n")
        docker.chmod(0o700)
        output = root / "result.json"
        process = subprocess.run([
            sys.executable, str(SCRIPTS / "ci_evidence.py"), "--delegated",
            "--gate", "docker", "--output", str(output), "--",
            sys.executable, str(SCRIPTS / "m8_container.py"), "rottweiler-m8-1-2", "--",
            "docker", "run", "--rm", "--name", "rottweiler-m8-1-2", "fixture", "false",
        ], env=dict(os.environ, PATH=str(root) + ":" + os.environ["PATH"],
                    DOCKER_LOG=str(root / "calls")), capture_output=True, timeout=20, check=False)
        return process, json.loads(output.read_text()), [json.loads(line) for line in (root / "calls").read_text().splitlines()]

    def test_exit_status_and_exact_container_removal_precede_ack(self):
        with tempfile.TemporaryDirectory() as temporary:
            process, evidence, calls = self.invoke(Path(temporary), "removed")
            self.assertEqual(process.returncode, 7, process.stderr.decode())
            self.assertNotIn("cleanup_error", evidence)
            self.assertIn(["rm", "--force", IDENTITY], calls)
            self.assertEqual(calls[-1], ["container", "ls", "--all", "--no-trunc", "--filter", f"id={IDENTITY}", "--format", "{{.ID}}"])
            self.assertNotIn("--rm", calls[0])

    def test_denied_or_unproven_removal_cannot_acknowledge(self):
        for behavior in ("denied", "retained"):
            with self.subTest(behavior=behavior), tempfile.TemporaryDirectory() as temporary:
                process, evidence, _ = self.invoke(Path(temporary), behavior)
                self.assertNotEqual(process.returncode, 0)
                self.assertIn("UNSETTLED", evidence["cleanup_error"])
