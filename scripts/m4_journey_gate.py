"""Real native approval, tools, interruption, child results, compaction and resume."""
from __future__ import annotations

import http.server
import json
import os
from pathlib import Path
import select
import socket
import threading
import time

from journal_observer import observed_envelopes, session_journals
from m4_gate_support import (
    DRIVER_READY_MARKER, FixtureHandler, TERMINAL_SUBMIT, descendant_pids,
    process_exists, spawn_pty, stop_pty, terminate_process_tree,
    wait_for_pty_exit, write_config,
)
from m4_terminal_screen import TerminalScreen
from perf_process import check_sample_cancellation

EDIT = "NATIVE_JOURNEY_EDIT"
INTERRUPT = "NATIVE_JOURNEY_INTERRUPT"
CHILDREN = "NATIVE_JOURNEY_CHILDREN"
OBSERVE = "NATIVE_JOURNEY_OBSERVE"
RESUME = "NATIVE_JOURNEY_RESUME"
SUMMARY = "NATIVE_JOURNEY_SUMMARY: journey.txt is after; test passed; two children finished."
CHILD_TASKS = ("NATIVE_CHILD_JOB_1", "NATIVE_CHILD_JOB_2")


def tool(call_id: str, name: str, arguments: dict) -> dict:
    return {"index": 0, "id": call_id, "type": "function",
            "function": {"name": name, "arguments": json.dumps(arguments)}}


def route_request(body: dict) -> tuple[str, object]:
    """Route only actual current user input and committed tool result IDs."""
    messages = body.get("messages", [])
    if body.get("tool_choice") == "none":
        if "Name this coding session" in json.dumps(messages):
            return "text", "Native approval and child journey"
        if "Create a hand-off summary" not in json.dumps(messages):
            raise ValueError("unexpected tool-free inference request")
        return "text", SUMMARY
    commands = (EDIT, INTERRUPT, CHILDREN, OBSERVE, RESUME, *CHILD_TASKS)
    current = next((command for message in reversed(messages)
                    if message.get("role") == "user"
                    for command in commands if command in json.dumps(message.get("content"))), None)
    returned = {message.get("tool_call_id") for message in messages if message.get("role") == "tool"}
    if current == EDIT:
        if "journey-edit" not in returned:
            return "tools", [tool("journey-edit", "edit", {"path": "journey.txt", "old": "before", "new": "after"})]
        if "journey-test" not in returned:
            return "tools", [tool("journey-test", "bash", {"command": 'test "$(cat journey.txt)" = after && printf NATIVE_TEST_OK', "cwd": "."})]
        return "text", "NATIVE_EDIT_AND_TEST_DONE"
    if current == INTERRUPT:
        return "hold", "NATIVE_INTERRUPT_ACTIVE"
    if current == CHILDREN:
        if not {"journey-child-1", "journey-child-2"} <= returned:
            calls = [tool(f"journey-child-{index}", "spawn_agent", {
                "action": "start", "task": task, "agent": "explore", "isolation": "shared",
            }) for index, task in enumerate(CHILD_TASKS, 1)]
            for index, call in enumerate(calls):
                call["index"] = index
            return "tools", calls
        return "text", "NATIVE_PARENT_CONTINUED"
    if current in CHILD_TASKS:
        return "child", CHILD_TASKS.index(current)
    if current == OBSERVE:
        serialized = json.dumps(messages)
        if not all(f"NATIVE_CHILD_RESULT_{index}" in serialized for index in (1, 2)):
            raise ValueError("parent request omitted durable child completion context")
        return "text", "NATIVE_PARENT_READ_CHILD_RESULTS"
    if current == RESUME:
        if SUMMARY not in json.dumps(messages):
            raise ValueError("resumed provider request omitted compacted summary")
        return "text", "NATIVE_RESUMED_FROM_SUMMARY"
    raise ValueError("provider received an unrecognized journey request")


class JourneyProvider(FixtureHandler):
    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("Content-Length", "0"))
        if self.path != "/v1/chat/completions" or not 0 < length <= 2 * 1024 * 1024:
            self.send_error(400)
            return
        try:
            body = json.loads(self.rfile.read(length))
            with self.server.lock:
                if len(self.server.requests) >= 40:
                    raise ValueError("journey request bound exceeded")
                self.server.requests.append(body)
            action, value = route_request(body)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            if action == "tools":
                self.frame({"tool_calls": value}, "tool_calls")
            elif action == "text":
                self.frame({"content": value}, "stop")
            else:
                marker = value if action == "hold" else f"NATIVE_CHILD_ACTIVE_{value + 1}"
                self.frame({"content": marker}, None)
                if action == "child":
                    self.server.children_started[value].set()
                self.connection.settimeout(.05)
                while not self.server.release.is_set():
                    if action == "child" and self.server.release_children.is_set():
                        self.frame({"content": f" NATIVE_CHILD_RESULT_{value + 1}"}, "stop")
                        return
                    try:
                        if not self.connection.recv(1):
                            if action == "hold":
                                self.server.interrupted.set()
                            return
                    except socket.timeout:
                        continue
        except (BrokenPipeError, ConnectionResetError):
            return
        except Exception as error:
            with self.server.lock:
                self.server.errors.append(str(error))
            self.close_connection = True

    def frame(self, delta: dict, finish: str | None) -> None:
        frame = {"id": "native-journey", "model": "gpt-5-mini",
                 "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
        payload = "data: " + json.dumps(frame) + "\n\n"
        if finish:
            payload += "data: [DONE]\n\n"
        self.wfile.write(payload.encode())
        self.wfile.flush()


def new_server():
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), JourneyProvider)
    server.lock = threading.Lock()
    server.requests, server.errors = [], []
    server.release = threading.Event()
    server.release_children = threading.Event()
    server.interrupted = threading.Event()
    server.children_started = [threading.Event(), threading.Event()]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, thread


class Journey:
    def __init__(self, rw, home, workspace, env, dimensions, server):
        self.rw, self.home, self.workspace, self.env = rw, home, workspace, env
        self.dimensions, self.server = dimensions, server
        self.process = None
        self.screen = TerminalScreen(*dimensions)
        self.screens = {}
        self.parent_journal = None
        self.observed_children = set()

    def start(self, resume=None):
        self.screen = TerminalScreen(*self.dimensions)
        args = ["--dangerously-trust", "--permission-mode", "strict"]
        if resume:
            args += ["--resume", resume]
        self.process = spawn_pty(self.rw, self.env, self.workspace, args, dimensions=self.dimensions)
        # The marker is a transport readiness receipt, not a visible screen assertion.
        self.wait(lambda: DRIVER_READY_MARKER.decode() in self.raw, "driver_ready")

    def pump(self):
        ready, _, _ = select.select([self.process.fd], [], [], .02)
        if ready:
            try:
                chunk = os.read(self.process.fd, 65536)
            except OSError:
                chunk = b""
            if chunk:
                self.raw = (self.raw + chunk.decode(errors="replace"))[-131072:]
                self.screen.feed(chunk)
        self.observed_children.update(descendant_pids(self.process.pid))

    def wait(self, predicate, phase, timeout=20):
        deadline = time.monotonic() + timeout
        if not hasattr(self, "raw"):
            self.raw = ""
        while time.monotonic() < deadline:
            check_sample_cancellation()
            if self.server.errors:
                raise RuntimeError(f"phase={phase}; provider={self.server.errors}")
            if predicate():
                self.screens[phase] = self.screen.text
                return
            self.pump()
        raise RuntimeError(f"phase={phase}; screen={self.screen.text!r}")

    def visible(self, text, phase):
        self.wait(lambda: text in self.screen.text, phase)

    def enter(self, text):
        os.write(self.process.fd, b"\x1b[200~" + text.encode() + b"\x1b[201~")
        self.visible(text, "echo_" + text)
        os.write(self.process.fd, TERMINAL_SUBMIT)

    def events(self):
        if self.parent_journal is None:
            for journal in session_journals(self.home / "sessions"):
                events = [entry["event"] for entry in observed_envelopes(journal)]
                if any(event.get("type") == "user_message_accepted" and event.get("content") == EDIT for event in events):
                    self.parent_journal = journal
                    return events
            return []
        return [entry["event"] for entry in observed_envelopes(self.parent_journal)]

    def event(self, kind, **fields):
        return any(event.get("type") == kind and all(event.get(key) == value for key, value in fields.items())
                   for event in self.events())

    def approve(self, call_id):
        self.wait(lambda: self.event("tool_approval_needed", tool_call_id=call_id), "approval_" + call_id)
        self.visible("Allow once", "visible_approval_" + call_id)
        if call_id == "journey-test":
            self.visible("printf", "test_command_review")
        os.write(self.process.fd, b"y")
        self.wait(lambda: self.event("tool_approval_resolved", tool_call_id=call_id, decision="allow_once"), "approved_" + call_id)
        self.wait(lambda: self.event("tool_call_finished", tool_call_id=call_id, is_error=False), "tool_" + call_id)

    def close(self):
        self.enter("/exit")
        status = wait_for_pty_exit(self.process, timeout=35)
        if os.waitstatus_to_exitcode(status) != 0:
            raise RuntimeError("journey supervisor exited unsuccessfully")
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            alive = [pid for pid in self.observed_children if process_exists(pid)]
            leaves = list((self.home / "run").glob("engine-*"))
            if not alive and not leaves:
                break
            time.sleep(.01)
        else:
            raise RuntimeError(f"journey cleanup leaked processes={alive}, runtime={leaves}")
        self.process.reap()
        self.process.prove_settled()
        self.process.close()
        self.process = None
        self.raw = ""
        self.observed_children.clear()

    def run(self):
        self.start()
        self.enter(EDIT)
        self.approve("journey-edit")
        if (self.workspace / "journey.txt").read_text() != "after\n":
            raise RuntimeError("approved edit did not change actual workspace file")
        self.approve("journey-test")
        self.visible("NATIVE_EDIT_AND_TEST_DONE", "edit_and_test_done")
        if not any(event.get("type") == "tool_call_finished" and event.get("tool_call_id") == "journey-test"
                   and "NATIVE_TEST_OK" in json.dumps(event) for event in self.events()):
            raise RuntimeError("native test command did not produce its successful result")
        self.enter(INTERRUPT)
        self.visible("NATIVE_INTERRUPT_ACTIVE", "interrupt_active")
        os.write(self.process.fd, b"\x03")
        self.wait(lambda: self.event("turn_finished", status="interrupted") and self.server.interrupted.is_set(), "interrupted")
        self.enter(CHILDREN)
        self.visible("NATIVE_PARENT_CONTINUED", "parent_continues")
        self.wait(lambda: all(event.is_set() for event in self.server.children_started), "both_children_running")
        if self.event("subagent_finished"):
            raise RuntimeError("parent did not continue while both children were running")
        self.server.release_children.set()
        self.wait(lambda: len([event for event in self.events() if event.get("type") == "subagent_finished"]) == 2, "children_finished")
        if any(event.get("result", {}).get("status") != "completed" for event in self.events() if event.get("type") == "subagent_finished"):
            raise RuntimeError("a background child failed")
        self.enter(OBSERVE)
        self.visible("NATIVE_PARENT_READ_CHILD_RESULTS", "parent_observes_results")
        self.enter("/compact")
        self.wait(lambda: self.event("compaction_finished"), "compacted", timeout=30)
        parent = self.parent_journal.parent.name
        self.close()
        self.start(parent)
        self.enter(RESUME)
        self.visible("NATIVE_RESUMED_FROM_SUMMARY", "resumed_summary")
        self.close()
        events = self.events()
        child_sessions = [event["child_session_id"] for event in events if event.get("type") == "subagent_spawned"]
        for child in child_sessions:
            journal = self.home / "sessions" / child / "journal"
            child_events = [entry["event"] for entry in observed_envelopes(journal)]
            if not any(event.get("type") == "turn_finished" and event.get("status") == "completed" for event in child_events):
                raise RuntimeError("child did not durably settle its own turn")
        return {"dimensions": self.dimensions, "session": parent, "events": events,
                "provider_requests": self.server.requests, "screens": self.screens,
                "workspace_text": (self.workspace / "journey.txt").read_text()}


def native_journey_gate(rw: Path, root: Path, isolated_env, evidence_directory: Path | None = None) -> None:
    for dimensions in ((110, 32), (80, 24)):
        label = f"journey-{dimensions[0]}x{dimensions[1]}"
        home, workspace = root / (label + "-home"), root / (label + "-workspace")
        workspace.mkdir(mode=0o700)
        (workspace / "journey.txt").write_text("before\n")
        server, thread = new_server()
        write_config(home, server.server_address[1])
        env = isolated_env(home)
        env["ROTTWEILER_DRIVER_READY_MARKER"] = DRIVER_READY_MARKER.decode()
        origin = f"http://127.0.0.1:{server.server_address[1]}"
        env.update(HTTPS_PROXY=origin, HTTP_PROXY=origin, NO_PROXY="127.0.0.1,localhost")
        journey = Journey(rw, home, workspace, env, dimensions, server)
        evidence = None
        try:
            evidence = journey.run()
            print(f"M4 native journey {dimensions[0]}x{dimensions[1]}: approvals, edit, test, interrupt, "
                  "two live children, parent completion context, compacted resume, clean shutdown")
        finally:
            server.release.set()
            try:
                if journey.process is not None:
                    terminate_process_tree(journey.process)
                    stop_pty(journey.process)
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=2)
                destination = evidence_directory or Path(os.environ.get("ROTTWEILER_M4_JOURNEY_EVIDENCE", str(root)))
                destination.mkdir(parents=True, exist_ok=True)
                (destination / (label + ".json")).write_text(json.dumps(evidence or {
                    "provider_requests": server.requests, "errors": server.errors,
                    "screens": journey.screens, "events": journey.events(),
                }, indent=2) + "\n")
