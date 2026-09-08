"""A stalled console must not hold the native process retirement owner."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
from ci_console import ConsoleRelay


class ConsoleTests(unittest.TestCase):
    def test_full_pipe_counts_omission_and_restores_descriptor_mode(self):
        reader, writer = os.pipe()
        try:
            relay = ConsoleRelay(writer)
            offered = 0
            while relay.omitted == 0:
                relay.write(b'x' * 4096)
                offered += 4096
            relay.write(b'blocked')
            offered += 7
            relay.close()
            self.assertTrue(os.get_blocking(writer))
            os.close(writer)
            writer = None
            retained = bytearray()
            while chunk := os.read(reader, 4096):
                retained.extend(chunk)
            self.assertEqual(len(retained) + relay.omitted, offered)
        finally:
            os.close(reader)
            if writer is not None:
                os.close(writer)

    def test_blocked_console_cancel_reaps_child_and_retains_evidence(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            pidfile, output = root / 'pid', root / 'result.json'
            # The independent fixture has a bounded fallback lifetime even if
            # cancellation regresses; it creates no descendants.
            child = ("import os,threading,time\n"
                     "threading.Timer(6,lambda: os._exit(9)).start()\n"
                     f"open({str(pidfile)!r},'w').write(str(os.getpid()))\n"
                     "os.write(1,b'early marker\\n'+b'x'*262144+b'\\nfinal marker\\n')\n"
                     "time.sleep(60)\n")
            process = subprocess.Popen([sys.executable, str(SCRIPTS / 'ci_evidence.py'),
                '--gate', 'blocked-console', '--output', str(output), '--', sys.executable, '-c', child],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                deadline = time.monotonic() + 3
                while not output.with_suffix('.log').exists() or output.with_suffix('.log').stat().st_size < 262144:
                    if time.monotonic() >= deadline:
                        self.fail('console congestion blocked retained output')
                    time.sleep(.01)
                process.send_signal(signal.SIGTERM)
                # Deliberately do not drain stdout until physical shutdown.
                self.assertEqual(process.wait(timeout=3), 130)
                with self.assertRaises(ProcessLookupError):
                    os.kill(int(pidfile.read_text()), 0)
                evidence = json.loads(output.read_text())
                self.assertGreater(evidence['console_omitted_bytes'], 0)
                self.assertNotIn('cleanup_error', evidence)
                self.assertIn('early marker', output.with_suffix('.log').read_text())
                self.assertIn('final marker', evidence['log_tail'])
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=3)
                process.communicate(timeout=3)
