#!/usr/bin/env python3
"""Offline M4 release-path performance and crash/replay acceptance gate."""

from __future__ import annotations

import argparse
import contextlib
import fcntl
import http.server
import json
import math
import os
import pathlib
import pty
import select
import shutil
import signal
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import threading
import time
import termios
from dataclasses import dataclass


FIRST_PAINT_MARKER = b"Rottweiler"
OPEN_TUI_FRAME_MARKER = b"ROTTWEILER_OPEN_TUI_FIRST_FRAME"
DRIVER_READY_MARKER = b"ROTTWEILER_TUI_DRIVER_READY"
PROMPT_MARKER = "M4_REATTACH_PROMPT_7f40"
RESPONSE_MARKER = "M4_REATTACH_RESPONSE_34d1"
SHELL_READY_MARKER = "M4_SHELL_CHILD_READY_f003"
SHELL_STDIN_MARKER = "M4_SHELL_CHILD_STDIN_0a19"
SHELL_INTERRUPT_MARKER = "M4_SHELL_CHILD_INTERRUPT_82bc"
BLOCKED_TURN_MARKER = "M4_BLOCKED_AGENT_TURN_6d77"
SHELL_SECRET_VALUE = "M4_SHELL_SECRET_d10f7e62"
# OpenTUI's multiline Textarea maps Meta+Return to submit; plain Return inserts
# a newline. Kitty encodes the meta modifier as the 1-based value 3.
KITTY_SUBMIT = b"\x1b[13;3u"
_origin_request_lock = threading.Lock()
_origin_requests = 0


@dataclass
class Runtime:
    process: subprocess.Popen[bytes]
    socket_path: pathlib.Path
    token_path: pathlib.Path
    stderr_path: pathlib.Path


@dataclass
class PtyProcess:
    pid: int
    fd: int


class UnixHttpConnection:
    """Small HTTP/1.1 client used to exercise hyper over the real UDS."""

    def __init__(self, socket_path: pathlib.Path, timeout: float = 5.0) -> None:
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.settimeout(timeout)
        self.socket.connect(str(socket_path))
        self.buffer = bytearray()

    def close(self) -> None:
        self.socket.close()

    def send_request(
        self,
        method: str,
        path: str,
        headers: dict[str, str] | None = None,
        body: bytes = b"",
    ) -> None:
        request_headers = {
            "Host": "rottweiler.local",
            "Content-Length": str(len(body)),
            "Connection": "keep-alive",
            **(headers or {}),
        }
        wire = bytearray(f"{method} {path} HTTP/1.1\r\n".encode("ascii"))
        for name, value in request_headers.items():
            wire.extend(f"{name}: {value}\r\n".encode("ascii"))
        wire.extend(b"\r\n")
        wire.extend(body)
        self.socket.sendall(wire)

    def read_response(self) -> tuple[int, dict[str, str], bytes]:
        header_end = self._receive_until(b"\r\n\r\n")
        header_block = bytes(self.buffer[:header_end])
        del self.buffer[: header_end + 4]
        lines = header_block.split(b"\r\n")
        try:
            status = int(lines[0].split(b" ", 2)[1])
        except (IndexError, ValueError) as error:
            raise RuntimeError(f"malformed HTTP response status: {lines[0]!r}") from error
        response_headers: dict[str, str] = {}
        for line in lines[1:]:
            name, separator, value = line.partition(b":")
            if not separator:
                raise RuntimeError(f"malformed HTTP response header: {line!r}")
            response_headers[name.decode("ascii").lower()] = value.decode("ascii").strip()

        if "content-length" in response_headers:
            length = int(response_headers["content-length"])
            self._receive_bytes(length)
            body = bytes(self.buffer[:length])
            del self.buffer[:length]
        elif response_headers.get("transfer-encoding", "").lower() == "chunked":
            body = self._read_chunked_body()
        else:
            raise RuntimeError("persistent HTTP response omitted a body length")
        return status, response_headers, body

    def _receive_until(self, marker: bytes) -> int:
        while True:
            found = self.buffer.find(marker)
            if found >= 0:
                return found
            self._receive_more()

    def _receive_bytes(self, length: int) -> None:
        while len(self.buffer) < length:
            self._receive_more()

    def _receive_more(self) -> None:
        chunk = self.socket.recv(65536)
        if not chunk:
            raise RuntimeError("HTTP connection closed before the response completed")
        self.buffer.extend(chunk)

    def _read_chunked_body(self) -> bytes:
        body = bytearray()
        while True:
            line_end = self._receive_until(b"\r\n")
            size_line = bytes(self.buffer[:line_end])
            del self.buffer[: line_end + 2]
            try:
                size = int(size_line.split(b";", 1)[0], 16)
            except ValueError as error:
                raise RuntimeError(f"invalid HTTP chunk size: {size_line!r}") from error
            if size == 0:
                trailer_end = self._receive_until(b"\r\n")
                del self.buffer[: trailer_end + 2]
                return bytes(body)
            self._receive_bytes(size + 2)
            body.extend(self.buffer[:size])
            if self.buffer[size : size + 2] != b"\r\n":
                raise RuntimeError("HTTP chunk omitted its terminator")
            del self.buffer[: size + 2]


class UnixSseStream:
    """Incremental SSE reader with HTTP chunk decoding."""

    def __init__(
        self,
        socket_path: pathlib.Path,
        client_id: str,
        token: str,
        timeout: float = 5.0,
    ) -> None:
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.settimeout(timeout)
        self.socket.connect(str(socket_path))
        self.raw = bytearray()
        self.decoded = bytearray()
        self.chunked = False
        self.chunk_remaining: int | None = None
        request = (
            "GET /v1/events HTTP/1.1\r\n"
            "Host: rottweiler.local\r\n"
            f"Authorization: Bearer {token}\r\n"
            f"x-rottweiler-client: {client_id}\r\n"
            "Accept: text/event-stream\r\n"
            "Connection: keep-alive\r\n\r\n"
        ).encode("ascii")
        self.socket.sendall(request)
        header_end = self._raw_until(b"\r\n\r\n")
        header_block = bytes(self.raw[:header_end])
        del self.raw[: header_end + 4]
        lines = header_block.split(b"\r\n")
        try:
            status = int(lines[0].split(b" ", 2)[1])
        except (IndexError, ValueError) as error:
            raise RuntimeError(f"malformed SSE HTTP status: {lines[0]!r}") from error
        if status != 200:
            raise RuntimeError(f"engine SSE subscription returned HTTP {status}")
        response_headers = {
            name.decode("ascii").lower(): value.decode("ascii").strip().lower()
            for line in lines[1:]
            for name, separator, value in [line.partition(b":")]
            if separator
        }
        content_type = response_headers.get("content-type", "")
        if not content_type.startswith("text/event-stream"):
            raise RuntimeError(f"engine SSE returned {content_type!r}, not text/event-stream")
        self.chunked = response_headers.get("transfer-encoding") == "chunked"

    def close(self) -> None:
        self.socket.close()

    def next_matching_event(
        self, request_id: str, expected_type: str, timeout: float = 5.0
    ) -> dict[str, object]:
        deadline = time.monotonic() + timeout
        self.socket.settimeout(timeout)
        while time.monotonic() < deadline:
            frame = self._next_frame()
            data = b"\n".join(
                line[5:].lstrip()
                for line in frame.replace(b"\r", b"").split(b"\n")
                if line.startswith(b"data:")
            )
            if not data:
                continue
            event = json.loads(data)
            meta = event.get("meta")
            if (
                isinstance(meta, dict)
                and meta.get("request_id") == request_id
                and event.get("type") == expected_type
            ):
                return event
        raise RuntimeError(
            f"SSE stream did not emit {expected_type!r} for request {request_id!r}"
        )

    def _next_frame(self) -> bytes:
        while True:
            frame_end = self.decoded.find(b"\n\n")
            if frame_end >= 0:
                frame = bytes(self.decoded[:frame_end])
                del self.decoded[: frame_end + 2]
                return frame
            self._decode_more()

    def _decode_more(self) -> None:
        if not self.chunked:
            self.decoded.extend(self._recv())
            return
        while True:
            if self.chunk_remaining is None:
                line_end = self.raw.find(b"\r\n")
                if line_end < 0:
                    self.raw.extend(self._recv())
                    continue
                size_line = bytes(self.raw[:line_end])
                del self.raw[: line_end + 2]
                self.chunk_remaining = int(size_line.split(b";", 1)[0], 16)
                if self.chunk_remaining == 0:
                    raise RuntimeError("engine closed the SSE response")
            required = self.chunk_remaining + 2
            if len(self.raw) < required:
                self.raw.extend(self._recv())
                continue
            self.decoded.extend(self.raw[: self.chunk_remaining])
            if self.raw[self.chunk_remaining : required] != b"\r\n":
                raise RuntimeError("SSE HTTP chunk omitted its terminator")
            del self.raw[:required]
            self.chunk_remaining = None
            return

    def _raw_until(self, marker: bytes) -> int:
        while True:
            found = self.raw.find(marker)
            if found >= 0:
                return found
            self.raw.extend(self._recv())

    def _recv(self) -> bytes:
        chunk = self.socket.recv(65536)
        if not chunk:
            raise RuntimeError("engine closed the SSE stream")
        return chunk


class FixtureHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self) -> None:  # noqa: N802 - stdlib callback name
        global _origin_requests
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        if self.path != "/v1/chat/completions":
            self.send_error(404)
            return
        with _origin_request_lock:
            _origin_requests += 1
        body = (
            "data: "
            + json.dumps(
                {
                    "id": "m4-release-fixture",
                    "model": "fixture-model",
                    "choices": [
                        {
                            "index": 0,
                            "delta": {"content": RESPONSE_MARKER},
                            "finish_reason": "stop",
                        }
                    ],
                },
                separators=(",", ":"),
            )
            + "\n\ndata: [DONE]\n\n"
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def log_message(self, _format: str, *_args: object) -> None:
        return


@contextlib.contextmanager
def fixture_origin():
    global _origin_requests
    with _origin_request_lock:
        _origin_requests = 0
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FixtureHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server.server_address[1]
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


def origin_request_count() -> int:
    with _origin_request_lock:
        return _origin_requests


def write_config(home: pathlib.Path, port: int) -> None:
    home.mkdir(mode=0o700, parents=True, exist_ok=True)
    config = f"""
[models]
default = "fast"
aliases.fast = ["fixture/gpt-5-mini"]

[providers.fixture]
kind = "openai_chat"
base_url = "http://127.0.0.1:{port}/v1/chat/completions"
api_key_env = "M4_FIXTURE_API_KEY"

[permissions]
default = "ask"
""".lstrip()
    path = home / "config.toml"
    path.write_text(config, encoding="utf-8")
    path.chmod(0o600)


def isolated_env(home: pathlib.Path, tui: pathlib.Path | None = None) -> dict[str, str]:
    env = {
        "HOME": str(home),
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "ROTTWEILER_HOME": str(home),
        "ROTTWEILER_CREDENTIAL_BACKEND": "file",
        "M4_FIXTURE_API_KEY": SHELL_SECRET_VALUE,
        "TERM": "xterm-256color",
        "COLORTERM": "truecolor",
        "NO_COLOR": "1",
    }
    if tui is not None:
        env["ROTTWEILER_TUI_BIN"] = str(tui)
    return env


def start_engine(
    rw: pathlib.Path,
    sample_root: pathlib.Path,
    workspace: pathlib.Path,
    port: int,
    session_id: str,
) -> tuple[Runtime, float]:
    runtime, started = spawn_engine(rw, sample_root, workspace, port, session_id)
    wait_for_health(runtime)
    ready_ms = (time.perf_counter_ns() - started) / 1_000_000
    return runtime, ready_ms


def spawn_engine(
    rw: pathlib.Path,
    sample_root: pathlib.Path,
    workspace: pathlib.Path,
    port: int,
    session_id: str,
) -> tuple[Runtime, int]:
    home = sample_root / "home"
    run = sample_root / "run"
    run.mkdir(mode=0o700, parents=True)
    write_config(home, port)
    socket_path = run / "engine.sock"
    token_path = run / "auth.token"
    stderr_path = sample_root / "engine.stderr"
    stderr = stderr_path.open("wb")
    started = time.perf_counter_ns()
    process = subprocess.Popen(
        [
            str(rw),
            "serve",
            "--socket",
            str(socket_path),
            "--token-file",
            str(token_path),
            "--session",
            session_id,
            "--workspace",
            str(workspace),
            "--permission-mode",
            "strict",
            "--model",
            "fast",
        ],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=stderr,
        cwd=workspace,
        env=isolated_env(home),
        start_new_session=True,
    )
    stderr.close()
    return Runtime(process, socket_path, token_path, stderr_path), started


def wait_for_health(runtime: Runtime, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        if runtime.process.poll() is not None:
            detail = runtime.stderr_path.read_text(encoding="utf-8", errors="replace")
            raise RuntimeError(
                f"release engine exited before readiness ({runtime.process.returncode}): {detail}"
            )
        try:
            token = runtime.token_path.read_text(encoding="ascii").strip()
            if len(token) != 64:
                raise RuntimeError("bootstrap token is not complete")
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                client.settimeout(0.2)
                client.connect(str(runtime.socket_path))
                client.sendall(
                    (
                        "GET /v1/health HTTP/1.1\r\n"
                        "Host: rottweiler.local\r\n"
                        f"Authorization: Bearer {token}\r\n"
                        "Connection: close\r\n\r\n"
                    ).encode()
                )
                response = client.recv(4096)
            if b" 200 " in response and b'{"ready":true}' in response:
                return
        except (FileNotFoundError, ConnectionError, OSError, RuntimeError) as error:
            last_error = error
        time.sleep(0.0005)
    raise RuntimeError(f"engine health endpoint was not ready: {last_error}")


def spawn_pty(
    executable: pathlib.Path,
    env: dict[str, str],
    cwd: pathlib.Path,
    arguments: list[str] | None = None,
) -> PtyProcess:
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        os.execve(str(executable), [str(executable), *(arguments or [])], env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    return PtyProcess(pid, fd)


def read_until(process: PtyProcess, marker: bytes, timeout: float = 5.0) -> bytes:
    deadline = time.monotonic() + timeout
    captured = bytearray()
    while time.monotonic() < deadline:
        ready, _, _ = select.select([process.fd], [], [], min(0.05, deadline - time.monotonic()))
        if not ready:
            continue
        try:
            chunk = os.read(process.fd, 65536)
        except OSError:
            break
        if not chunk:
            break
        captured.extend(chunk)
        if marker in captured:
            return bytes(captured)
        if len(captured) > 4 * 1024 * 1024:
            del captured[: len(captured) - 2 * 1024 * 1024]
    child_status = "still running"
    with contextlib.suppress(ChildProcessError):
        found, status = os.waitpid(process.pid, os.WNOHANG)
        if found == process.pid:
            child_status = f"exited with wait status {status}"
    raise RuntimeError(
        f"PTY process {process.pid} did not render marker {marker!r} ({child_status}); "
        f"tail={bytes(captured[-1000:])!r}"
    )


def stop_pty(process: PtyProcess) -> None:
    with contextlib.suppress(ProcessLookupError):
        # OpenTUI owns raw-mode teardown and does not promise a SIGTERM exit.
        # The measurement is already complete; SIGKILL avoids adding a fixed
        # two-second cleanup penalty to every cold-start sample.
        os.kill(process.pid, signal.SIGKILL)
    with contextlib.suppress(ChildProcessError):
        os.waitpid(process.pid, 0)
    with contextlib.suppress(OSError):
        os.close(process.fd)


def terminate_process_tree(root_pid: int, timeout: float = 3.0) -> None:
    descendants = descendant_pids(root_pid)
    for pid in [*reversed(descendants), root_pid]:
        with contextlib.suppress(ProcessLookupError):
            os.kill(pid, signal.SIGTERM)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        alive = [pid for pid in [root_pid, *descendants] if process_exists(pid)]
        if not alive:
            break
        time.sleep(0.01)
    for pid in [*reversed(descendants), root_pid]:
        with contextlib.suppress(ProcessLookupError):
            os.kill(pid, signal.SIGKILL)
    with contextlib.suppress(ChildProcessError):
        os.waitpid(root_pid, 0)


def descendant_pids(root_pid: int) -> list[int]:
    output = subprocess.check_output(["ps", "-axo", "pid=,ppid="], text=True)
    by_parent: dict[int, list[int]] = {}
    for line in output.splitlines():
        fields = line.split()
        if len(fields) == 2:
            by_parent.setdefault(int(fields[1]), []).append(int(fields[0]))
    descendants: list[int] = []
    pending = list(by_parent.get(root_pid, []))
    while pending:
        pid = pending.pop()
        descendants.append(pid)
        pending.extend(by_parent.get(pid, []))
    return descendants


def process_exists(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


def stop_runtime(runtime: Runtime) -> None:
    if runtime.process.poll() is None:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(runtime.process.pid, signal.SIGTERM)
        try:
            runtime.process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(runtime.process.pid, signal.SIGKILL)
            runtime.process.wait(timeout=2)


def one_startup_sample(
    rw: pathlib.Path,
    tui: pathlib.Path,
    root: pathlib.Path,
    workspace: pathlib.Path,
    port: int,
    index: int,
) -> tuple[float, float]:
    sample_root = root / f"sample-{index}"
    sample_root.mkdir(mode=0o700)
    runtime, started = spawn_engine(
        rw, sample_root, workspace, port, f"m4-perf-{index}"
    )
    tui_process: PtyProcess | None = None
    try:
        env = isolated_env(sample_root / "home")
        env.update(
            {
                "ROTTWEILER_ENGINE_SOCKET": str(runtime.socket_path),
                "ROTTWEILER_ENGINE_TOKEN_FILE": str(runtime.token_path),
                "ROTTWEILER_SESSION_ID": f"m4-perf-{index}",
                "ROTTWEILER_LAST_SEEN_FILE": str(sample_root / "run" / "last-seen"),
                "ROTTWEILER_FIRST_FRAME_MARKER": OPEN_TUI_FRAME_MARKER.decode("ascii"),
            }
        )
        tui_process = spawn_pty(tui, env, workspace)
        # Production supervision starts both children before waiting for the
        # engine handoff. Poll readiness while OpenTUI loads so neither cold
        # start is hidden and the total measures their real concurrent path.
        wait_for_health(runtime)
        ready_ms = (time.perf_counter_ns() - started) / 1_000_000
        read_until(tui_process, OPEN_TUI_FRAME_MARKER)
        combined_ms = (time.perf_counter_ns() - started) / 1_000_000
        return ready_ms, combined_ms
    finally:
        if tui_process is not None:
            stop_pty(tui_process)
        stop_runtime(runtime)


def percentile(values: list[float], quantile: float) -> float:
    ordered = sorted(values)
    return ordered[max(0, min(len(ordered) - 1, math.ceil(len(ordered) * quantile) - 1))]


def performance_gate(
    rw: pathlib.Path,
    tui: pathlib.Path,
    root: pathlib.Path,
    workspace: pathlib.Path,
    port: int,
    samples: int,
) -> None:
    one_startup_sample(rw, tui, root, workspace, port, -1)
    measurements = [
        one_startup_sample(rw, tui, root, workspace, port, index)
        for index in range(samples)
    ]
    engine = [measurement[0] for measurement in measurements]
    combined = [measurement[1] for measurement in measurements]
    engine_p99 = percentile(engine, 0.99)
    combined_p99 = percentile(combined, 0.99)
    print(
        "M4 release startup: "
        f"samples={samples}; engine_ms p50={statistics.median(engine):.3f} "
        f"p99={engine_p99:.3f} max={max(engine):.3f}; "
        f"engine_plus_tui_first_paint_ms p50={statistics.median(combined):.3f} "
        f"p99={combined_p99:.3f} max={max(combined):.3f}"
    )
    if engine_p99 >= 50:
        raise RuntimeError(f"engine-ready p99 {engine_p99:.3f}ms exceeds 50ms")
    if combined_p99 >= 150:
        raise RuntimeError(
            f"cold engine plus compiled-TUI first-paint p99 {combined_p99:.3f}ms exceeds 150ms"
        )


def mint_client(runtime: Runtime) -> tuple[str, str]:
    bootstrap = runtime.token_path.read_text(encoding="ascii").strip()
    connection = UnixHttpConnection(runtime.socket_path)
    try:
        connection.send_request(
            "POST",
            "/v1/connect",
            {"Authorization": f"Bearer {bootstrap}"},
        )
        status, _, body = connection.read_response()
    finally:
        connection.close()
    if status != 201:
        raise RuntimeError(f"engine client mint returned HTTP {status}: {body!r}")
    credentials = json.loads(body)
    client_id = credentials.get("client_id")
    token = credentials.get("token")
    if not isinstance(client_id, str) or not isinstance(token, str):
        raise RuntimeError("engine client mint returned malformed credentials")
    return client_id, token


def open_event_stream(
    runtime: Runtime, client_id: str, token: str, timeout: float = 5.0
) -> UnixSseStream:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            return UnixSseStream(runtime.socket_path, client_id, token, timeout)
        except (ConnectionError, OSError, RuntimeError) as error:
            last_error = error
            time.sleep(0.001)
    raise RuntimeError(f"production host event stream was not ready: {last_error}")


def socket_latency_gate(
    rw: pathlib.Path,
    root: pathlib.Path,
    workspace: pathlib.Path,
    port: int,
    samples: int,
) -> None:
    sample_root = root / "socket-latency"
    sample_root.mkdir(mode=0o700)
    session_id = "m4-socket-latency"
    runtime, _ = start_engine(rw, sample_root, workspace, port, session_id)
    events: UnixSseStream | None = None
    commands: UnixHttpConnection | None = None
    try:
        client_id, token = mint_client(runtime)
        # Receiving the production SSE response headers proves that the real
        # HostedEngine subscription exists; the health socket itself comes up
        # before deferred session composition has finished.
        events = open_event_stream(runtime, client_id, token)
        commands = UnixHttpConnection(runtime.socket_path)
        headers = {
            "Authorization": f"Bearer {token}",
            "x-rottweiler-client": client_id,
            "Content-Type": "application/json",
        }
        query_types = [
            ("list_commands", "command_descriptors_listed"),
            ("list_models", "models_listed"),
        ]

        def run_query(index: int, measured: bool) -> float:
            command_type, event_type = query_types[index % len(query_types)]
            request_id = f"m4-latency-{'sample' if measured else 'warmup'}-{index}"
            command = json.dumps(
                {
                    "type": command_type,
                    "meta": {
                        "protocol_version": 1,
                        # The server must overwrite this with the authenticated
                        # identity before dispatching the typed command.
                        "client_id": "transport-spoof",
                        "request_id": request_id,
                    },
                },
                separators=(",", ":"),
            ).encode("utf-8")
            started = time.perf_counter_ns()
            commands.send_request("POST", "/v1/command", headers, command)
            event = events.next_matching_event(request_id, event_type)
            elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
            meta = event.get("meta")
            if not isinstance(meta, dict) or meta.get("client_id") != client_id:
                raise RuntimeError(
                    "host result did not carry the transport-bound client identity"
                )
            status, _, response = commands.read_response()
            if status != 202:
                raise RuntimeError(
                    f"typed host query {command_type!r} returned HTTP {status}: {response!r}"
                )
            outcome = json.loads(response)
            if outcome.get("type") != "accepted":
                raise RuntimeError(
                    f"typed host query {command_type!r} was not accepted: {outcome!r}"
                )
            return elapsed_ms

        # Warm the persistent UDS connection, host query registry, pricing
        # metadata, SSE chunk decoder, and Python pages before measuring p99.
        for index in range(min(50, max(10, samples // 10))):
            run_query(index, False)
        latencies = [run_query(index, True) for index in range(samples)]
        latency_p99 = percentile(latencies, 0.99)
        print(
            "M4 production engine-to-TUI UDS event latency: "
            f"samples={samples}; p50={statistics.median(latencies):.3f}ms "
            f"p99={latency_p99:.3f}ms max={max(latencies):.3f}ms; "
            "typed_queries=list_commands,list_models"
        )
        if latency_p99 >= 2:
            raise RuntimeError(
                f"production engine-to-TUI socket event p99 {latency_p99:.3f}ms exceeds 2ms"
            )
    finally:
        if commands is not None:
            commands.close()
        if events is not None:
            events.close()
        stop_runtime(runtime)


def child_processes(parent: int) -> list[tuple[int, str]]:
    output = subprocess.check_output(["ps", "-axo", "pid=,ppid=,command="], text=True)
    children: list[tuple[int, str]] = []
    for line in output.splitlines():
        fields = line.strip().split(maxsplit=2)
        if len(fields) == 3 and int(fields[1]) == parent:
            children.append((int(fields[0]), fields[2]))
    return children


def wait_for_tui_child(parent: int, executable: pathlib.Path, exclude: int | None = None) -> int:
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        for pid, command in child_processes(parent):
            if pid != exclude and str(executable) in command:
                return pid
        time.sleep(0.01)
    raise RuntimeError("supervisor did not expose the compiled TUI child")


def wait_for_engine_child(
    parent: int, executable: pathlib.Path, exclude: int | None = None
) -> int:
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        for pid, command in child_processes(parent):
            if (
                pid != exclude
                and str(executable) in command
                and " serve" in command
            ):
                return pid
        time.sleep(0.01)
    raise RuntimeError("supervisor did not expose the engine child")


def supervisor_reattach_gate(
    rw: pathlib.Path,
    tui: pathlib.Path,
    root: pathlib.Path,
    workspace: pathlib.Path,
    port: int,
) -> None:
    home = root / "supervisor-home"
    write_config(home, port)
    process = spawn_pty(rw, isolated_env(home, tui), workspace)
    try:
        read_until(process, FIRST_PAINT_MARKER, timeout=8)
        first_tui = wait_for_tui_child(process.pid, tui)
        os.write(process.fd, PROMPT_MARKER.encode() + KITTY_SUBMIT)
        first_transcript = read_until(process, RESPONSE_MARKER.encode(), timeout=8)
        if PROMPT_MARKER.encode() not in first_transcript:
            raise RuntimeError("first TUI did not render the submitted user message")

        os.kill(first_tui, signal.SIGKILL)
        second_tui = wait_for_tui_child(process.pid, tui, exclude=first_tui)
        if second_tui == first_tui:
            raise RuntimeError("supervisor did not replace the SIGKILLed TUI process")
        rebuilt = read_until(process, RESPONSE_MARKER.encode(), timeout=8)
        if PROMPT_MARKER.encode() not in rebuilt:
            raise RuntimeError("reattached TUI omitted the durable user message")
        print(
            "M4 supervisor reattach: actual compiled TUI was SIGKILLed and the replacement "
            "re-rendered the complete durable prompt/response transcript"
        )
    finally:
        terminate_process_tree(process.pid)
        with contextlib.suppress(OSError):
            os.close(process.fd)


def shell_handover_gate(
    rw: pathlib.Path,
    tui: pathlib.Path,
    root: pathlib.Path,
    workspace: pathlib.Path,
    port: int,
) -> None:
    home = root / "shell-home"
    write_config(home, port)
    child_script = workspace / "m4-shell-child.py"
    child_script.write_text(
        "#!/usr/bin/env python3\n"
        "import signal\n"
        "import os\n"
        "import sys\n"
        f"READY = {SHELL_READY_MARKER!r}\n"
        f"STDIN = {SHELL_STDIN_MARKER!r}\n"
        f"INTERRUPT = {SHELL_INTERRUPT_MARKER!r}\n"
        "SECRET = os.environ['M4_FIXTURE_API_KEY']\n"
        "def interrupted(_signal, _frame):\n"
        "    print(f'{INTERRUPT}:{SECRET}', flush=True)\n"
        "    raise SystemExit(23)\n"
        "signal.signal(signal.SIGINT, interrupted)\n"
        "print(READY, flush=True)\n"
        "for line in sys.stdin:\n"
        "    print(f'{STDIN}:{line.rstrip()}', flush=True)\n",
        encoding="utf-8",
    )
    child_script.chmod(0o700)

    shell_env = isolated_env(home, tui)
    shell_env["ROTTWEILER_DRIVER_READY_MARKER"] = DRIVER_READY_MARKER.decode("ascii")
    process = spawn_pty(rw, shell_env, workspace)
    try:
        read_until(process, FIRST_PAINT_MARKER, timeout=8)
        first_engine = wait_for_engine_child(process.pid, rw)
        baseline_requests = origin_request_count()
        os.write(process.fd, f"!{child_script}".encode() + KITTY_SUBMIT)
        read_until(process, SHELL_READY_MARKER.encode(), timeout=8)

        # The child and parent-side PTY broker must survive an engine crash.
        # Completion later remints against the rotated token and confirms the
        # recovered matching shell end before allowing the TUI to resume.
        os.kill(first_engine, signal.SIGKILL)
        second_engine = wait_for_engine_child(process.pid, rw, exclude=first_engine)
        if second_engine == first_engine:
            raise RuntimeError("supervisor did not replace the crashed engine")
        read_until(process, DRIVER_READY_MARKER, timeout=8)

        # The compiled TUI is suspended and the child owns input. An attempted
        # agent prompt must be consumed by the foreground child, not start a
        # provider turn while the durable shell-active gate is set.
        os.write(process.fd, BLOCKED_TURN_MARKER.encode() + b"\n")
        child_input = read_until(process, SHELL_STDIN_MARKER.encode(), timeout=3)
        if BLOCKED_TURN_MARKER.encode() not in child_input:
            raise RuntimeError("foreground child did not own the attempted agent input")
        time.sleep(0.05)
        if origin_request_count() != baseline_requests:
            raise RuntimeError("agent provider turn started while foreground shell was active")

        # The controlling terminal sends SIGINT to rw; the broker must forward
        # it to the foreground child's process group.
        os.write(process.fd, b"\x03")
        read_until(process, SHELL_INTERRUPT_MARKER.encode(), timeout=3)
        read_until(process, b"Message Rottweiler", timeout=5)
        # Normal agent execution resumes only after durable shell completion.
        os.write(process.fd, PROMPT_MARKER.encode() + KITTY_SUBMIT)
        read_until(process, RESPONSE_MARKER.encode(), timeout=8)
        if origin_request_count() != baseline_requests + 1:
            raise RuntimeError("agent turn did not resume exactly once after shell completion")

        shell_events = durable_shell_events(home)
        if len(shell_events) != 2:
            raise RuntimeError(f"expected one durable shell start/end pair, got {shell_events!r}")
        started, ended = shell_events
        if not started.get("active") or ended.get("active"):
            raise RuntimeError("durable shell-active gate did not bracket the real child")
        if ended.get("status") != 23:
            raise RuntimeError(f"Ctrl+C child exit status was not preserved: {ended!r}")
        captured = ended.get("captured_output")
        if not isinstance(captured, str):
            raise RuntimeError("foreground output was not persisted")
        if SHELL_SECRET_VALUE in captured or "[REDACTED]" not in captured:
            raise RuntimeError(
                "known foreground secret was not redacted before durable persistence"
            )
        if SHELL_STDIN_MARKER not in captured or SHELL_INTERRUPT_MARKER not in captured:
            raise RuntimeError("usable non-secret foreground output was lost from the transcript")
        if int(started["meta"]["sequence_id"]) >= int(ended["meta"]["sequence_id"]):
            raise RuntimeError("shell completion was not durably ordered after shell start")
        print(
            "M4 foreground TTY: child survived engine/token restart, owned input, blocked "
            "agent execution, received Ctrl+C, and persisted a redacted output tail before "
            "the TUI resumed"
        )
    finally:
        terminate_process_tree(process.pid)
        with contextlib.suppress(OSError):
            os.close(process.fd)


def durable_shell_events(home: pathlib.Path) -> list[dict[str, object]]:
    event_logs = list((home / "sessions").glob("*/events.jsonl"))
    if len(event_logs) != 1:
        raise RuntimeError(f"expected one durable session event log, found {event_logs!r}")
    events: list[dict[str, object]] = []
    for line in event_logs[0].read_text(encoding="utf-8").splitlines():
        envelope = json.loads(line)
        event = envelope.get("event")
        if isinstance(event, dict) and event.get("type") == "user_shell_state_changed":
            events.append(event)
    return events


def wait_pid(pid: int, timeout: float) -> int:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        found, status = os.waitpid(pid, os.WNOHANG)
        if found == pid:
            return status
        time.sleep(0.01)
    with contextlib.suppress(ProcessLookupError):
        os.kill(pid, signal.SIGKILL)
    return os.waitpid(pid, 0)[1]


def ssh_preflight(host: str) -> None:
    completed = subprocess.run(
        ["/usr/bin/ssh", "-T", "-o", "BatchMode=yes", "--", host, "true"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        timeout=5,
        check=False,
    )
    if completed.returncode != 0:
        raise RuntimeError(
            f"SSH loopback preflight failed for {host}: "
            + completed.stderr.decode(errors="replace").strip()
        )


def ssh_loopback_gate(
    rw: pathlib.Path,
    tui: pathlib.Path,
    root: pathlib.Path,
    workspace: pathlib.Path,
    port: int,
    host: str,
) -> None:
    ssh_preflight(host)
    home = root / "ssh-home"
    write_config(home, port)
    wrapper = root / "remote-rw"
    wrapper.write_text(
        "#!/bin/sh\n"
        f"export ROTTWEILER_HOME={home}\n"
        "export ROTTWEILER_CREDENTIAL_BACKEND=file\n"
        f"exec {rw} \"$@\"\n",
        encoding="utf-8",
    )
    wrapper.chmod(0o700)
    env = isolated_env(home, tui)
    env.update(
        {
            "ROTTWEILER_REMOTE_RW": str(wrapper),
            "ROTTWEILER_SSH_BIN": "/usr/bin/ssh",
        }
    )
    local = spawn_pty(rw, env, workspace, ["--permission-mode", "strict"])
    try:
        read_until(local, FIRST_PAINT_MARKER, timeout=10)
        os.write(local.fd, PROMPT_MARKER.encode() + KITTY_SUBMIT)
        local_capture = read_until(local, RESPONSE_MARKER.encode(), timeout=10)
        require_visible_markers(local_capture)
        local_transcript = wait_for_canonical_durable_transcript(home)
    finally:
        terminate_process_tree(local.pid)
        with contextlib.suppress(OSError):
            os.close(local.fd)

    session_id = "m4-ssh-loopback-gate"
    remote = spawn_pty(
        rw,
        env,
        workspace,
        [
            "--remote",
            host,
            "--permission-mode",
            "strict",
            "--resume",
            session_id,
        ],
    )
    try:
        read_until(remote, FIRST_PAINT_MARKER, timeout=10)
        os.write(remote.fd, PROMPT_MARKER.encode() + KITTY_SUBMIT)
        remote_capture = read_until(remote, RESPONSE_MARKER.encode(), timeout=10)
        require_visible_markers(remote_capture)
        remote_transcript = wait_for_canonical_durable_transcript(home, session_id)
        if remote_transcript != local_transcript:
            raise RuntimeError(
                f"loopback transcript bytes differ: local={local_transcript!r} "
                f"remote={remote_transcript!r}"
            )
        print(
            "M4 SSH loopback: production rw --remote path rendered the byte-identical "
            "canonical durable user/assistant transcript through StreamLocal forwarding"
        )
    finally:
        terminate_process_tree(remote.pid)
        with contextlib.suppress(OSError):
            os.close(remote.fd)
        cleanup_detached_remote(session_id)


def require_visible_markers(captured: bytes) -> None:
    prompt = captured.find(PROMPT_MARKER.encode())
    response = captured.find(RESPONSE_MARKER.encode(), max(0, prompt))
    if prompt < 0 or response < 0 or response < prompt:
        raise RuntimeError("TUI capture omitted the ordered prompt/response transcript")


def canonical_durable_transcript(
    home: pathlib.Path, session_id: str | None = None
) -> bytes:
    if session_id is None:
        event_logs = list((home / "sessions").glob("*/events.jsonl"))
        if len(event_logs) != 1:
            raise RuntimeError(
                f"expected one local durable transcript, found {event_logs!r}"
            )
        event_log = event_logs[0]
    else:
        event_log = home / "sessions" / session_id / "events.jsonl"
        if not event_log.is_file():
            raise RuntimeError(f"remote durable transcript is missing: {event_log}")

    turns: list[dict[str, object]] = []
    for line in event_log.read_text(encoding="utf-8").splitlines():
        envelope = json.loads(line)
        event = envelope.get("event")
        if not isinstance(event, dict) or event.get("type") != "conversation_turn_committed":
            continue
        turn = event.get("turn")
        if not isinstance(turn, dict):
            raise RuntimeError("durable conversation event omitted its typed turn")
        role = turn.get("role")
        blocks = turn.get("blocks")
        if not isinstance(role, str) or not isinstance(blocks, list):
            raise RuntimeError("durable conversation turn has an invalid protocol shape")
        # Session ids, sequence ids, timestamps, and provider bookkeeping are
        # intentionally excluded; role and provider-neutral blocks are the
        # canonical transcript bytes both local and remote clients must share.
        turns.append({"role": role, "blocks": blocks})
    canonical = json.dumps(
        turns, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")
    if PROMPT_MARKER.encode() not in canonical or RESPONSE_MARKER.encode() not in canonical:
        raise RuntimeError(
            f"durable transcript omitted fixture prompt/response blocks: {canonical!r}"
        )
    return canonical


def wait_for_canonical_durable_transcript(
    home: pathlib.Path, session_id: str | None = None, timeout: float = 5.0
) -> bytes:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            return canonical_durable_transcript(home, session_id)
        except (FileNotFoundError, RuntimeError, json.JSONDecodeError) as error:
            last_error = error
            time.sleep(0.001)
    raise RuntimeError(f"durable transcript did not settle: {last_error}")


def cleanup_detached_remote(session_id: str) -> None:
    root = pathlib.Path(f"/tmp/rottweiler-{os.geteuid()}")
    for descriptor in root.glob("engine-*/runtime.json"):
        try:
            value = json.loads(descriptor.read_text(encoding="utf-8"))
            if value.get("session_id") != session_id:
                continue
            pid = int(value["pid"])
        except (OSError, ValueError, KeyError, json.JSONDecodeError):
            continue
        with contextlib.suppress(ProcessLookupError):
            os.kill(pid, signal.SIGTERM)
        directory = descriptor.parent
        deadline = time.monotonic() + 2
        while process_exists(pid) and time.monotonic() < deadline:
            time.sleep(0.01)
        with contextlib.suppress(ProcessLookupError):
            os.kill(pid, signal.SIGKILL)
        for path in [directory / "engine.sock", directory / "auth.token", descriptor]:
            with contextlib.suppress(FileNotFoundError):
                path.unlink()
        with contextlib.suppress(OSError):
            directory.rmdir()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", type=pathlib.Path, required=True)
    parser.add_argument("--rw", type=pathlib.Path, required=True)
    parser.add_argument("--tui", type=pathlib.Path, required=True)
    parser.add_argument("--samples", type=int, default=100)
    parser.add_argument("--skip-performance", action="store_true")
    parser.add_argument("--skip-supervisor", action="store_true")
    parser.add_argument("--skip-shell", action="store_true")
    parser.add_argument("--ssh-loopback", metavar="HOST")
    return parser.parse_args()


def opentui_native_library_name() -> str:
    if sys.platform == "darwin":
        return "libopentui.dylib"
    if sys.platform == "win32":
        return "opentui.dll"
    return "libopentui.so"


def main() -> int:
    args = parse_args()
    if args.samples < 100 and not args.skip_performance:
        raise RuntimeError("p99 release gate requires at least 100 samples")
    repo = args.repo.resolve()
    source_rw = args.rw.resolve()
    source_tui = args.tui.resolve()
    source_tui_native = source_tui.with_name(opentui_native_library_name())
    if not source_rw.is_file() or not source_tui.is_file() or not source_tui_native.is_file():
        raise RuntimeError(
            "release rw, compiled TUI, and sibling OpenTUI native library must exist"
        )
    # Darwin's sockaddr_un path is only 104 bytes. Keep the release harness
    # rooted at the short /tmp spelling so the production supervisor's nested
    # private runtime directory is testing startup rather than path overflow.
    with tempfile.TemporaryDirectory(prefix="rw4-", dir="/tmp") as temporary:
        root = pathlib.Path(temporary)
        root.chmod(0o700)
        # Benchmark installed-artifact copies. Python's copyfile copies the
        # executable bytes but not macOS provenance/quarantine xattrs, matching
        # the M3 release harness and excluding one-time Gatekeeper scanning from
        # the product startup budget.
        artifact_bin = root / "bin"
        artifact_bin.mkdir(mode=0o700)
        rw = artifact_bin / "rw"
        tui = artifact_bin / "rottweiler-tui"
        tui_native = artifact_bin / source_tui_native.name
        shutil.copyfile(source_rw, rw)
        shutil.copyfile(source_tui, tui)
        shutil.copyfile(source_tui_native, tui_native)
        rw.chmod(0o700)
        tui.chmod(0o700)
        workspace = root / "workspace"
        workspace.mkdir(mode=0o700)
        with fixture_origin() as port:
            if not args.skip_performance:
                performance_gate(rw, tui, root, workspace, port, args.samples)
                socket_latency_gate(rw, root, workspace, port, args.samples)
            if not args.skip_supervisor:
                supervisor_reattach_gate(rw, tui, root, workspace, port)
            if not args.skip_shell:
                shell_handover_gate(rw, tui, root, workspace, port)
            if args.ssh_loopback is not None:
                ssh_loopback_gate(rw, tui, root, workspace, port, args.ssh_loopback)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:  # noqa: BLE001 - gate prints one actionable failure
        print(f"M4 release gate failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
