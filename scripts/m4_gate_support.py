"""Offline provider, local transport, process, and evidence support for the M4 gate."""
from __future__ import annotations

import contextlib
import fcntl
import http.server
import json
import os
import pathlib
import pty
import re
import select
import signal
import socket
import struct
import subprocess
import tempfile
import threading
import time
import termios
from dataclasses import dataclass

FIRST_PAINT_MARKER = b"Rottweiler"

TUI_PROCESS_START_MARKER = b"ROTTWEILER_TUI_PROCESS_START"

TUI_TRANSCRIPT_PAINTED_MARKER = b"ROTTWEILER_TUI_TRANSCRIPT_PAINTED"

TUI_INTERACTIVE_MARKER = b"ROTTWEILER_TUI_INTERACTIVE"

DRIVER_READY_MARKER = b"ROTTWEILER_TUI_DRIVER_READY"

PROMPT_MARKER = "M4_REATTACH_PROMPT_7f40"

RESPONSE_MARKER = "M4_REATTACH_RESPONSE_34d1"

SHELL_READY_MARKER = "M4_SHELL_CHILD_READY_f003"

SHELL_STDIN_MARKER = "M4_SHELL_CHILD_STDIN_0a19"

SHELL_INTERRUPT_MARKER = "M4_SHELL_CHILD_INTERRUPT_82bc"

SHELL_EXIT_MARKER = "Shell · exited 23"

BLOCKED_TURN_MARKER = "M4_BLOCKED_AGENT_TURN_6d77"

SHELL_SECRET_VALUE = "M4_SHELL_SECRET_d10f7e62"

REPRESENTATIVE_PRICING_MODEL_COUNT = 4_000

TERMINAL_SUBMIT = b"\r"

_origin_request_lock = threading.Lock()

_origin_requests = 0

_discovery_requests = 0

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

class GateEvidence:
    """Write observations before assertions; failed gates retain partial samples."""

    def __init__(self, output: pathlib.Path | None) -> None:
        self.output = output
        self.started = time.monotonic()
        self.samples: dict[str, list[dict[str, int]]] = {}
        self.result: dict[str, object] = {
            "schema_version": 1,
            "status": "running",
            "phase": "setup",
            "source_sha": os.environ.get("GITHUB_SHA"),
            "run_id": os.environ.get("GITHUB_RUN_ID"),
            "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
            "samples": self.samples,
        }

    def update(self, **fields: object) -> None:
        self.result.update(fields)
        self.result["elapsed_seconds"] = round(time.monotonic() - self.started, 3)
        if self.output is not None:
            self.output.parent.mkdir(parents=True, exist_ok=True)
            descriptor, temporary = tempfile.mkstemp(prefix=f".{self.output.name}.", dir=self.output.parent)
            try:
                with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
                    json.dump(self.result, handle, sort_keys=True)
                    handle.write("\n")
                os.replace(temporary, self.output)
            finally:
                with contextlib.suppress(FileNotFoundError):
                    os.unlink(temporary)

    def sample(self, group: str, **values: int) -> None:
        self.samples.setdefault(group, []).append(values)
        self.update()

    def failure(self, error: BaseException) -> None:
        self.update(
            status="fail",
            error_type=type(error).__name__,
            error=str(error).replace(SHELL_SECRET_VALUE, "[REDACTED]")[-8_000:],
            fixture_discoveries=discovery_request_count(),
            fixture_completions=origin_request_count(),
        )

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

    def do_GET(self) -> None:  # noqa: N802 - stdlib callback name
        global _discovery_requests
        if self.path != "/v1/models":
            self.send_error(404)
            return
        if self.headers.get("Authorization") != f"Bearer {SHELL_SECRET_VALUE}":
            self.send_error(401)
            return
        with _origin_request_lock:
            _discovery_requests += 1
        body = json.dumps(
            {"object": "list", "data": [{"id": "gpt-5-mini", "object": "model"}]},
            separators=(",", ":"),
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

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
                    "model": "gpt-5-mini",
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
    global _origin_requests, _discovery_requests
    with _origin_request_lock:
        _origin_requests = 0
        _discovery_requests = 0
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

def discovery_request_count() -> int:
    with _origin_request_lock:
        return _discovery_requests

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
    write_representative_pricing_catalog(home / "models.toml")

def write_representative_pricing_catalog(path: pathlib.Path) -> None:
    """Seed the gate with the catalog size of a used installation."""
    entries = [
        'source_url = "https://models.dev/api.json"',
        'snapshot_date = "2026-08-22"',
        'revision = "m4-representative-fixture-v1"',
    ]
    for index in range(REPRESENTATIVE_PRICING_MODEL_COUNT):
        model = "gpt-5-mini" if index == 0 else f"synthetic-{index:04d}"
        entries.extend(
            [
                "",
                f'[models."fixture/{model}"]',
                f'display_name = "M4 fixture model {index:04d}"',
                "max_context_tokens = 128000",
                "max_output_tokens = 16384",
                "supports_tools = true",
                "supports_thinking = true",
                'reasoning_efforts = ["low", "medium", "high"]',
                "input_per_million_micros_usd = 250000",
                "output_per_million_micros_usd = 2000000",
            ]
        )
    path.write_text("\n".join(entries) + "\n", encoding="utf-8")
    path.chmod(0o600)

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

def spawn_wrapped_pty(
    executable: pathlib.Path,
    env: dict[str, str],
    cwd: pathlib.Path,
    arguments: list[str] | None = None,
) -> PtyProcess:
    """Keep the PTY session leader alive while its supervised child is killed."""
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        child = os.fork()
        if child == 0:
            os.execve(str(executable), [str(executable), *(arguments or [])], env)
        os.waitpid(child, 0)
        while True:
            signal.pause()
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    return PtyProcess(pid, fd)

def read_until(
    process: PtyProcess, marker: bytes, timeout: float = 5.0, *, phase: str = "render"
) -> bytes:
    return read_until_all(process, (marker,), timeout, phase=phase)

def read_until_all(
    process: PtyProcess, markers: tuple[bytes, ...], timeout: float = 5.0,
    *, phase: str = "render",
) -> bytes:
    if not markers or any(not marker for marker in markers):
        raise ValueError("PTY markers must be non-empty")
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
        if all(marker in captured for marker in markers):
            return bytes(captured)
        if len(captured) > 4 * 1024 * 1024:
            del captured[: len(captured) - 2 * 1024 * 1024]
    child_status = "still running"
    with contextlib.suppress(ChildProcessError):
        found, status = os.waitpid(process.pid, os.WNOHANG)
        if found == process.pid:
            child_status = f"exited with wait status {status}"
    terminal_tail = re.sub(
        r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\))",
        "",
        captured.decode("utf-8", errors="replace"),
    ).replace(SHELL_SECRET_VALUE, "[REDACTED]")[-4000:]
    raise RuntimeError(
        f"phase={phase}; PTY process {process.pid} did not render markers {markers!r} "
        f"({child_status}); fixture_discoveries={discovery_request_count()}; "
        f"fixture_completions={origin_request_count()}; "
        f"tail={terminal_tail!r}"
    )

def wait_for_pty_exit(process: PtyProcess, timeout: float) -> int:
    """Drain terminal teardown output while waiting for a PTY child to exit."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        found, status = os.waitpid(process.pid, os.WNOHANG)
        if found == process.pid:
            return status
        ready, _, _ = select.select(
            [process.fd], [], [], min(0.05, deadline - time.monotonic())
        )
        if ready:
            with contextlib.suppress(OSError):
                os.read(process.fd, 65536)
    raise TimeoutError(f"PTY process {process.pid} did not exit within {timeout} seconds")

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
