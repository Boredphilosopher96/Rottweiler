#!/usr/bin/env python3
"""Exercise the provider fixture and failure evidence without running benchmarks."""
from __future__ import annotations

import http.client
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

MODULE_PATH = Path(__file__).parents[2] / "crates/rw-cli/tests/m4_release_gate.py"
SPEC = importlib.util.spec_from_file_location("m4_release_fixture", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
M4 = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M4
SPEC.loader.exec_module(M4)


class M4FixtureTests(unittest.TestCase):
    def test_catalog_and_inference_share_the_configured_model(self) -> None:
        with M4.fixture_origin() as port:
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            self.addCleanup(connection.close)
            connection.request("GET", "/v1/models", headers={
                "Authorization": f"Bearer {M4.SHELL_SECRET_VALUE}",
            })
            response = connection.getresponse()
            self.assertEqual(response.status, 200)
            models = json.loads(response.read())
            self.assertEqual(models["data"][0]["id"], "gpt-5-mini")
            self.assertEqual(M4.discovery_request_count(), 1)
            self.assertEqual(M4.origin_request_count(), 0)
            connection.request("POST", "/v1/chat/completions", body=json.dumps({
                "model": models["data"][0]["id"], "messages": [],
            }), headers={"Authorization": f"Bearer {M4.SHELL_SECRET_VALUE}"})
            response = connection.getresponse()
            self.assertEqual(response.status, 200)
            self.assertIn(M4.RESPONSE_MARKER.encode(), response.read())
            self.assertEqual(M4.origin_request_count(), 1)

    def test_catalog_auth_failure_is_not_successful_discovery(self) -> None:
        with M4.fixture_origin() as port:
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            self.addCleanup(connection.close)
            connection.request("GET", "/v1/models")
            response = connection.getresponse()
            self.assertEqual(response.status, 401)
            self.assertNotIn(M4.SHELL_SECRET_VALUE.encode(), response.read())
            self.assertEqual(M4.discovery_request_count(), 0)

    def test_failed_startup_budget_retains_every_measured_sample(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "evidence.json"
            evidence = M4.GateEvidence(output)
            evidence.update(phase="startup")
            with mock.patch.object(M4, "one_startup_sample", return_value=(51.0, 100.0, 300.0)), mock.patch.object(M4.time, "sleep"):
                with self.assertRaisesRegex(RuntimeError, "exceeds 50ms") as failure:
                    M4.performance_gate(Path("rw"), Path("tui"), Path(temporary), Path(temporary), 1, 100, evidence)
            evidence.failure(failure.exception)
            result = json.loads(output.read_text())
            self.assertEqual(result["status"], "fail")
            self.assertEqual(result["phase"], "startup")
            self.assertEqual(len(result["samples"]["startup"]), 100)
            self.assertEqual(result["samples"]["startup"][0]["engine_ready_us"], 51_000)
            self.assertEqual(list(output.parent.glob(".evidence.json.*")), [])

    def test_pty_failure_names_the_phase_and_redacts_fixture_credentials(self) -> None:
        with mock.patch.object(M4.select, "select", return_value=([1], [], [])), mock.patch.object(M4.os, "read", side_effect=[M4.SHELL_SECRET_VALUE.encode(), b""]), mock.patch.object(M4.os, "waitpid", return_value=(0, 0)):
            with self.assertRaises(RuntimeError) as failure:
                M4.read_until(M4.PtyProcess(123, 1), b"missing", phase="initial_input_echo")
        self.assertIn("phase=initial_input_echo", str(failure.exception))
        self.assertNotIn(M4.SHELL_SECRET_VALUE, str(failure.exception))
        self.assertIn("[REDACTED]", str(failure.exception))


if __name__ == "__main__":
    unittest.main()
