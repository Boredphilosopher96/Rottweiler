"""Installed-bundle shutdown while a real provider stream remains open."""
from __future__ import annotations

import http.server
import json
import os
from pathlib import Path
import signal
import socket
import threading
import time

from journal_observer import observed_envelopes, session_journals
from m4_gate_support import (
    DRIVER_READY_MARKER, FixtureHandler, TERMINAL_SUBMIT, descendant_pids,
    process_exists, read_until, spawn_pty, stop_pty, terminate_process_tree,
    wait_for_pty_exit, write_config,
)

STREAM_MARKER = b"SHUTDOWN_ACTIVE_STREAM_68d9"


def active_shutdown_gate(rw: Path, root: Path, workspace: Path, isolated_env) -> None:
    """Prove public exit and supervisor signals retire an active stream and children."""
    for width, height, action in ((w, h, action) for w, h in ((110, 32), (80, 24))
                                  for action in ("exit", "sigterm", "sighup")):
        disconnected = threading.Event()
        release = threading.Event()

        class StreamingHandler(FixtureHandler):
            def do_POST(self) -> None:  # noqa: N802
                self.rfile.read(int(self.headers.get("Content-Length", "0")))
                if self.path != "/v1/chat/completions":
                    self.send_error(404)
                    return
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                frame = {"id": "shutdown-stream", "model": "gpt-5-mini",
                         "choices": [{"index": 0, "delta": {
                             "content": STREAM_MARKER.decode()}, "finish_reason": None}]}
                self.wfile.write(("data: " + json.dumps(frame) + "\n\n").encode())
                self.wfile.flush()
                self.connection.settimeout(0.1)
                while not release.is_set():
                    try:
                        if not self.connection.recv(1):
                            disconnected.set()
                            return
                    except socket.timeout:
                        continue
                    except ConnectionError:
                        disconnected.set()
                        return

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), StreamingHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        process = None
        settled = False
        try:
            home = root / f"active-shutdown-{width}x{height}-{action}"
            write_config(home, server.server_address[1])
            env = isolated_env(home)
            env["ROTTWEILER_DRIVER_READY_MARKER"] = DRIVER_READY_MARKER.decode()
            process = spawn_pty(rw, env, workspace, ["--dangerously-trust"],
                                dimensions=(width, height))
            read_until(process, DRIVER_READY_MARKER, timeout=20, phase="shutdown_ready")
            prompt = b"Keep streaming until interrupted"
            os.write(process.fd, prompt)
            read_until(process, prompt, timeout=8, phase="shutdown_prompt")
            os.write(process.fd, TERMINAL_SUBMIT)
            read_until(process, STREAM_MARKER, timeout=10, phase="shutdown_active_stream")
            children = descendant_pids(process.pid)
            started = time.monotonic()
            if action == "exit":
                # One write preserves the argumented slash command as composer input.
                os.write(process.fd, b"\x1b[200~/exit\x1b[201~")
                read_until(process, b"/exit", timeout=3, phase="shutdown_exit_echo")
                os.write(process.fd, TERMINAL_SUBMIT)
            else:
                os.kill(process.pid, signal.SIGTERM if action == "sigterm" else signal.SIGHUP)
            status = wait_for_pty_exit(process, timeout=35)
            code = os.waitstatus_to_exitcode(status)
            if code != 0:
                raise RuntimeError(f"active {action} exited with {code}")
            elapsed = time.monotonic() - started
            deadline = started + 35
            while time.monotonic() < deadline:
                alive = [pid for pid in children if process_exists(pid)]
                leaves = list((home / "run").glob("engine-*"))
                if not alive and not leaves and disconnected.is_set():
                    break
                time.sleep(0.01)
            else:
                raise RuntimeError(f"active {action} leaked children={alive}, runtime={leaves}, "
                                   f"provider_disconnected={disconnected.is_set()}")
            events = [entry["event"] for journal in session_journals(home / "sessions")
                      for entry in observed_envelopes(journal)]
            started_turns = {event["turn_id"] for event in events if event.get("type") == "turn_started"}
            terminals = {event["turn_id"]: event for event in events if event.get("type") == "turn_finished"}
            if (not started_turns or started_turns != terminals.keys()
                    or any(terminals[turn].get("status") != "interrupted" for turn in started_turns)):
                raise RuntimeError(f"active {action} did not durably interrupt every started turn")
            process.reap()
            process.prove_settled()
            process.close()
            settled = True
            print(f"M4 active {action} {width}x{height}: stream disconnected, {len(children)} observed children "
                  f"retired, supervisor exited in {elapsed:.3f}s")
        finally:
            release.set()
            try:
                if process is not None and not settled:
                    terminate_process_tree(process)
                    stop_pty(process)
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=2)
