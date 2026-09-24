"""Fresh-home native setup through the visible TUI, with a loopback provider."""
from __future__ import annotations

import http.server
import json
import os
import select
from pathlib import Path
import threading
import time
import tomllib

from journal_observer import observed_envelopes, session_journals
from m4_gate_support import (
    DRIVER_READY_MARKER, RESPONSE_MARKER, FixtureHandler, TERMINAL_SUBMIT,
    descendant_pids, process_exists, spawn_pty, stop_pty,
    terminate_process_tree, wait_for_pty_exit,
)

from m4_terminal_screen import TerminalScreen
from perf_process import check_sample_cancellation

MODEL = "openai/gpt-5-mini"


class OnboardingProvider(FixtureHandler):
    def do_POST(self) -> None:  # noqa: N802
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        expected = self.server.expected_prompt
        user_texts = [message.get("content") for message in body.get("messages", [])
                      if message.get("role") == "user"]
        if (self.path != "/v1/chat/completions" or body.get("model") != MODEL
                or not any(expected in json.dumps(value) for value in user_texts)):
            self.send_error(400, "unexpected model or user prompt")
            return
        response = {"id": "onboarding", "model": MODEL, "choices": [{"index": 0,
                    "delta": {"content": RESPONSE_MARKER}, "finish_reason": "stop"}]}
        encoded = ("data: " + json.dumps(response) + "\n\ndata: [DONE]\n\n").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(encoded)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(encoded)

    def do_GET(self) -> None:  # noqa: N802
        if self.path != "/v1/models":
            self.send_error(404)
            return
        body = json.dumps({"data": [{"id": MODEL}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def onboarding_gate(rw: Path, root: Path, isolated_env) -> None:
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), OnboardingProvider)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        for width, height in ((110, 32), (80, 24)):
            home = root / f"onboarding-{width}x{height}"
            workspace = root / f"onboarding-workspace-{width}x{height}"
            home.mkdir(mode=0o700)
            workspace.mkdir(mode=0o700)
            env = isolated_env(home)
            env["ROTTWEILER_DRIVER_READY_MARKER"] = DRIVER_READY_MARKER.decode()
            origin = f"http://127.0.0.1:{server.server_address[1]}"
            # Metadata refresh cannot leave this fixture. CONNECT is rejected by
            # this HTTP server; direct loopback discovery/inference still works.
            env.update(HTTPS_PROXY=origin, HTTP_PROXY=origin, NO_PROXY="127.0.0.1,localhost")
            server.expected_prompt = f"NATIVE_FIRST_PROMPT_{width}_{height}"
            process = spawn_pty(rw, env, workspace, ["--dangerously-trust"],
                                dimensions=(width, height))
            settled = False
            terminal = TerminalScreen(width, height)
            try:
                def screen(marker: str, phase: str, timeout: float = 15) -> None:
                    deadline = time.monotonic() + timeout
                    while time.monotonic() < deadline:
                        check_sample_cancellation()
                        if marker in terminal.text:
                            return
                        ready, _, _ = select.select([process.fd], [], [], .05)
                        if ready:
                            try:
                                chunk = os.read(process.fd, 65536)
                            except OSError:
                                break
                            if not chunk:
                                break
                            terminal.feed(chunk)
                    raise RuntimeError(f"phase={phase}; screen missing {marker!r}; "
                                       f"screen={terminal.text!r}")

                def enter(text: str = "") -> None:
                    if text:
                        os.write(process.fd, text.encode())
                        screen(text, "onboarding_input_echo")
                    os.write(process.fd, TERMINAL_SUBMIT)

                screen("connect a provider", "onboarding_welcome")
                if (home / "config.toml").exists():
                    raise RuntimeError("fresh onboarding wrote configuration before selection")
                enter("compatible")
                screen("Provider name", "onboarding_provider_name")
                # This custom profile's canonical IDs exercise the bundled
                # OpenRouter metadata without pre-seeding a models.toml file.
                enter("openrouter")
                screen("API format", "onboarding_api_format")
                enter()
                screen("Full inference endpoint URL", "onboarding_endpoint")
                enter(origin + "/v1/chat/completions")
                screen("Authentication", "onboarding_authentication")
                os.write(process.fd, b"\x1b[B" + TERMINAL_SUBMIT)
                screen("Initial model", "onboarding_model_id")
                enter()
                screen("Save provider and connect", "onboarding_review")
                enter()
                screen("gpt-5-mini", "onboarding_activated")
                deadline = time.monotonic() + 15
                while True:
                    try:
                        config = tomllib.loads((home / "config.toml").read_text())
                        if config.get("models", {}).get("default") == f"openrouter/{MODEL}":
                            break
                    except FileNotFoundError:
                        pass
                    if time.monotonic() >= deadline:
                        raise RuntimeError("onboarding did not persist its initial model default")
                    time.sleep(.01)
                os.write(process.fd, b"\x1b")
                # Let the legacy terminal parser settle a standalone Escape;
                # adjoining prompt bytes would instead be an Alt key sequence.
                time.sleep(.1)
                prompt = f"NATIVE_FIRST_PROMPT_{width}_{height}"
                os.write(process.fd, prompt.encode())
                screen(prompt, "onboarding_prompt_echo")
                enter()
                screen(RESPONSE_MARKER, "onboarding_first_response")
                children = descendant_pids(process.pid)
                os.write(process.fd, b"\x1b[200~/exit\x1b[201~" + TERMINAL_SUBMIT)
                status = wait_for_pty_exit(process, timeout=35)
                if os.waitstatus_to_exitcode(status) != 0:
                    raise RuntimeError("onboarding session did not shut down successfully")
                deadline = time.monotonic() + 5
                while any(process_exists(pid) for pid in children) and time.monotonic() < deadline:
                    time.sleep(.01)
                if any(process_exists(pid) for pid in children) or list((home / "run").glob("engine-*")):
                    raise RuntimeError("onboarding session left owned processes/runtime behind")
                events = [entry["event"] for journal in session_journals(home / "sessions")
                          for entry in observed_envelopes(journal)]
                if not any(event.get("type") == "context_usage_updated"
                           and event.get("context_window_known") for event in events):
                    raise RuntimeError("first prompt lacked bundled context-window metadata")
                process.reap()
                process.prove_settled()
                process.close()
                settled = True
                print(f"M4 onboarding {width}x{height}: fresh home, visible endpoint setup, "
                      "automatic model/default, first reply, known context window, clean exit")
            finally:
                if not settled:
                    terminate_process_tree(process)
                    stop_pty(process)
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
