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
import re
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


sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[3] / "scripts"))
from journal_observer import observed_envelopes, session_journals
from m4_gate_support import (
    BLOCKED_TURN_MARKER,
    DRIVER_READY_MARKER,
    FIRST_PAINT_MARKER,
    FixtureHandler,
    GateEvidence,
    PROMPT_MARKER,
    PtyProcess,
    REPRESENTATIVE_PRICING_MODEL_COUNT,
    RESPONSE_MARKER,
    Runtime,
    SHELL_EXIT_MARKER,
    SHELL_INTERRUPT_MARKER,
    SHELL_READY_MARKER,
    SHELL_SECRET_VALUE,
    SHELL_STDIN_MARKER,
    TERMINAL_SUBMIT,
    TUI_INTERACTIVE_MARKER,
    TUI_PROCESS_START_MARKER,
    TUI_TRANSCRIPT_PAINTED_MARKER,
    UnixHttpConnection,
    UnixSseStream,
    descendant_pids,
    discovery_request_count,
    fixture_origin,
    origin_request_count,
    process_exists,
    read_until,
    read_until_all,
    spawn_pty,
    spawn_wrapped_pty,
    stop_pty,
    stop_runtime,
    terminate_process_tree,
    wait_for_pty_exit,
    write_config,
    write_representative_pricing_catalog,
)


# The gate drives an xterm-compatible PTY, so send the same carriage return a
# physical Return key produces there. Kitty's CSI-u encoding is only emitted by
# terminals after negotiating that protocol and is not portable PTY input.


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


def one_startup_sample(
    rw: pathlib.Path,
    tui: pathlib.Path,
    root: pathlib.Path,
    workspace: pathlib.Path,
    port: int,
    index: int,
) -> tuple[float, float, float]:
    sample_root = root / f"sample-{index}"
    sample_root.mkdir(mode=0o700)
    wall_started_ms = time.time_ns() / 1_000_000
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
                "ROTTWEILER_PROCESS_START_MARKER": TUI_PROCESS_START_MARKER.decode("ascii"),
                "ROTTWEILER_PROCESS_START_EPOCH": "1",
                "ROTTWEILER_TRANSCRIPT_PAINTED_MARKER": TUI_TRANSCRIPT_PAINTED_MARKER.decode("ascii"),
                "ROTTWEILER_INTERACTIVE_MARKER": TUI_INTERACTIVE_MARKER.decode("ascii"),
                "ROTTWEILER_INTERACTIVE_EPOCH": "1",
            }
        )
        tui_process = spawn_pty(tui, env, workspace)
        # Production supervision starts both children before waiting for the
        # engine handoff. Poll readiness while OpenTUI loads so neither cold
        # start is hidden and the total measures their real concurrent path.
        wait_for_health(runtime)
        ready_ms = (time.perf_counter_ns() - started) / 1_000_000
        captured = read_until(tui_process, TUI_PROCESS_START_MARKER)
        timestamp_prefix = TUI_PROCESS_START_MARKER + b":"
        try:
            emitted_at_ms = float(
                captured.split(timestamp_prefix, 1)[1].splitlines()[0].decode("ascii")
            )
        except (IndexError, UnicodeDecodeError, ValueError) as error:
            raise RuntimeError("TUI process-start marker omitted its emission timestamp") from error
        process_start_ms = emitted_at_ms - wall_started_ms
        if process_start_ms <= 0 or process_start_ms > 5_000:
            raise RuntimeError(
                f"TUI process-start timestamp was implausible: {process_start_ms}ms"
            )
        read_until(tui_process, TUI_TRANSCRIPT_PAINTED_MARKER)
        os.write(tui_process.fd, b"x")
        interactive = read_until(tui_process, TUI_INTERACTIVE_MARKER)
        timestamp_prefix = TUI_INTERACTIVE_MARKER + b":"
        try:
            interactive_at_ms = float(
                interactive.split(timestamp_prefix, 1)[1].splitlines()[0].decode("ascii")
            )
        except (IndexError, UnicodeDecodeError, ValueError) as error:
            raise RuntimeError("TUI interactive marker omitted its emission timestamp") from error
        interactive_ms = interactive_at_ms - wall_started_ms
        if interactive_ms <= 0 or interactive_ms > 5_000:
            raise RuntimeError(
                f"TUI interactive timestamp was implausible: {interactive_ms}ms"
            )
        return ready_ms, max(ready_ms, process_start_ms), max(ready_ms, interactive_ms)
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
    evidence: GateEvidence | None = None,
) -> dict[str, int]:
    # Warm the installed-artifact inode until macOS's one-time executable
    # inspection and dynamic-loader caches have settled. A single warmup can
    # return its first paint while XProtect is still scanning in the
    # background, contaminating later cold-process samples with installation
    # work that is explicitly outside this startup budget.
    for index in range(-5, 0):
        one_startup_sample(rw, tui, root, workspace, port, index)
    time.sleep(0.05)
    measurements = []
    for index in range(samples):
        measurement = one_startup_sample(rw, tui, root, workspace, port, index)
        measurements.append(measurement)
        if evidence is not None:
            evidence.sample(
                "startup",
                engine_ready_us=math.ceil(measurement[0] * 1000),
                tui_process_start_us=math.ceil(measurement[1] * 1000),
                tui_interactive_us=math.ceil(measurement[2] * 1000),
            )
    engine = [measurement[0] for measurement in measurements]
    process_start = [measurement[1] for measurement in measurements]
    interactive = [measurement[2] for measurement in measurements]
    engine_p99 = percentile(engine, 0.99)
    process_start_p99 = percentile(process_start, 0.99)
    interactive_p99 = percentile(interactive, 0.99)
    print(
        "M4 release startup: "
        f"samples={samples}; engine_ms p50={statistics.median(engine):.3f} "
        f"p99={engine_p99:.3f} max={max(engine):.3f}; "
        f"engine_plus_tui_process_start_ms p50={statistics.median(process_start):.3f} "
        f"p99={process_start_p99:.3f} max={max(process_start):.3f}; "
        f"tui_interactive_ms p50={statistics.median(interactive):.3f} "
        f"p99={interactive_p99:.3f} max={max(interactive):.3f}"
    )
    if engine_p99 >= 50:
        raise RuntimeError(f"engine-ready p99 {engine_p99:.3f}ms exceeds 50ms")
    if process_start_p99 >= 150:
        raise RuntimeError(
            f"cold engine plus compiled-TUI process-start p99 {process_start_p99:.3f}ms exceeds 150ms"
        )
    if interactive_p99 >= 500:
        raise RuntimeError(
            f"cold compiled-TUI interactive p99 {interactive_p99:.3f}ms exceeds 500ms"
        )
    return {
        "engine_ready_p99_us": math.ceil(engine_p99 * 1000),
        "tui_process_start_p99_us": math.ceil(process_start_p99 * 1000),
        "tui_interactive_p99_us": math.ceil(interactive_p99 * 1000),
    }


def installed_first_launch_gate(
    source_rw: pathlib.Path,
    source_tui: pathlib.Path,
    source_tui_native: pathlib.Path,
    root: pathlib.Path,
    workspace: pathlib.Path,
    port: int,
    samples: int,
    evidence: GateEvidence | None = None,
) -> dict[str, int]:
    """Measure the first execution of freshly written installed artifacts."""
    version: list[float] = []
    interactive: list[float] = []
    for index in range(samples):
        version_bin = root / f"installed-first-version-{index}" / "bin"
        version_bin.mkdir(mode=0o700, parents=True)
        version_rw = version_bin / "rw"
        shutil.copyfile(source_rw, version_rw)
        version_rw.chmod(0o700)
        started = time.perf_counter_ns()
        result = subprocess.run(
            [str(version_rw), "--version"],
            cwd=workspace,
            env=isolated_env(root / f"installed-first-version-home-{index}"),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        version.append((time.perf_counter_ns() - started) / 1_000_000)
        if evidence is not None:
            evidence.sample("installed_first_version", elapsed_us=math.ceil(version[-1] * 1000))
        if result.returncode != 0 or not result.stdout.startswith(b"rw "):
            raise RuntimeError(
                "freshly installed rw --version failed: "
                f"rc={result.returncode} stderr={result.stderr[-2000:]!r}"
            )

        artifact_bin = root / f"installed-first-interactive-{index}" / "bin"
        artifact_bin.mkdir(mode=0o700, parents=True)
        rw = artifact_bin / "rw"
        tui = artifact_bin / "rottweiler-tui"
        native = artifact_bin / source_tui_native.name
        for source, destination in [
            (source_rw, rw),
            (source_tui, tui),
            (source_tui_native, native),
        ]:
            shutil.copyfile(source, destination)
        rw.chmod(0o700)
        tui.chmod(0o700)
        measurement_root = root / f"installed-first-measurement-{index}"
        measurement_root.mkdir(mode=0o700)
        interactive.append(
            one_startup_sample(
                rw,
                tui,
                measurement_root,
                workspace,
                port,
                0,
            )[2]
        )
        if evidence is not None:
            evidence.sample("installed_first_interactive", elapsed_us=math.ceil(interactive[-1] * 1000))

    version_max = max(version)
    interactive_max = max(interactive)
    print(
        "M4 installed first launch: "
        f"samples={samples}; version_ms median={statistics.median(version):.3f} "
        f"max={version_max:.3f}; interactive_ms median={statistics.median(interactive):.3f} "
        f"max={interactive_max:.3f}"
    )
    if version_max >= 1_000:
        raise RuntimeError(
            f"installed first rw --version max {version_max:.3f}ms exceeds 1000ms"
        )
    if interactive_max >= 3_000:
        raise RuntimeError(
            f"installed first interactive max {interactive_max:.3f}ms exceeds 3000ms"
        )
    return {
        "installed_first_version_max_us": math.ceil(version_max * 1000),
        "installed_first_interactive_max_us": math.ceil(interactive_max * 1000),
    }


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
    evidence: GateEvidence | None = None,
) -> dict[str, int]:
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

        # Authenticated transport readiness deliberately precedes deferred
        # session composition. Command discovery became session-scoped once it
        # included trusted project and extension commands, so wait outside the
        # measured window until that actor is loaded. Rejections have no result
        # event; inspect the HTTP outcome before waiting on SSE to avoid turning
        # a typed startup state into an opaque socket timeout.
        ready_deadline = time.monotonic() + 5
        ready_attempt = 0
        while True:
            request_id = f"m4-latency-ready-{ready_attempt}"
            ready_attempt += 1
            command = json.dumps(
                {
                    "type": "list_commands",
                    "meta": {
                        "protocol_version": 1,
                        "client_id": "transport-spoof",
                        "request_id": request_id,
                    },
                    "session_id": session_id,
                },
                separators=(",", ":"),
            ).encode("utf-8")
            commands.send_request("POST", "/v1/command", headers, command)
            status, _, response = commands.read_response()
            if status != 202:
                raise RuntimeError(
                    f"session readiness query returned HTTP {status}: {response!r}"
                )
            outcome = json.loads(response)
            if outcome.get("type") == "accepted":
                events.next_matching_event(request_id, "command_descriptors_listed")
                break
            error = outcome.get("error")
            if (
                not isinstance(error, dict)
                or error.get("code") != "session_not_loaded"
                or time.monotonic() >= ready_deadline
            ):
                raise RuntimeError(
                    f"session did not become query-ready: {outcome!r}"
                )
            time.sleep(0.001)

        def run_query(index: int, measured: bool) -> float:
            command_type, event_type = query_types[index % len(query_types)]
            request_id = f"m4-latency-{'sample' if measured else 'warmup'}-{index}"
            command_payload: dict[str, object] = {
                "type": command_type,
                "meta": {
                    "protocol_version": 1,
                    # The server must overwrite this with the authenticated
                    # identity before dispatching the typed command.
                    "client_id": "transport-spoof",
                    "request_id": request_id,
                },
            }
            # Command discovery is session-scoped because its runtime registry
            # includes trusted project and extension commands. Keep the release
            # gate on the generated protocol instead of relying on the older
            # global-list shape.
            if command_type == "list_commands":
                command_payload["session_id"] = session_id
            command = json.dumps(command_payload, separators=(",", ":")).encode("utf-8")
            started = time.perf_counter_ns()
            commands.send_request("POST", "/v1/command", headers, command)
            event = events.next_matching_event(request_id, event_type)
            elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
            if measured and evidence is not None:
                evidence.sample("uds_query", elapsed_us=math.ceil(elapsed_ms * 1000))
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
        return {"uds_event_p99_us": math.ceil(latency_p99 * 1000)}
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


def wait_for_supervisor_child(parent: int, executable: pathlib.Path) -> int:
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        for pid, command in child_processes(parent):
            if str(executable) in command and " serve" not in command:
                return pid
        time.sleep(0.01)
    raise RuntimeError("PTY wrapper did not expose the supervisor child")


def model_discovery_gate(rw: pathlib.Path, root: pathlib.Path, workspace: pathlib.Path, port: int) -> None:
    home = root / "discovery-home"
    write_config(home, port)
    before = discovery_request_count()
    result = subprocess.run(
        [str(rw), "models", "list", "--refresh", "--output-format", "json"],
        cwd=workspace, env=isolated_env(home), capture_output=True, text=True,
        timeout=8, check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(f"fixture model discovery exited {result.returncode}: {result.stderr[-2000:]}")
    catalog = json.loads(result.stdout)
    available = [model["id"] for model in catalog["models"] if model["available"]]
    provider = next((provider for provider in catalog["providers"] if provider["name"] == "fixture"), None)
    if (
        available != ["fixture/gpt-5-mini"]
        or provider is None
        or not all(provider[field] for field in ("configured", "authenticated", "reachable"))
        or discovery_request_count() <= before
    ):
        raise RuntimeError("fixture discovery did not expose the authenticated configured model")
    print("M4 provider discovery: public CLI authenticated and discovered the configured fixture model")


def supervisor_reattach_gate(
    rw: pathlib.Path,
    tui: pathlib.Path,
    root: pathlib.Path,
    workspace: pathlib.Path,
    port: int,
) -> None:
    home = root / "supervisor-home"
    write_config(home, port)
    # This is an isolated, generated CI workspace. Opt into the product's
    # explicit non-persisting CI trust escape hatch so the fixture exercises
    # supervision instead of blocking at the interactive folder-trust prompt.
    runtime_root = home / "run"
    env = isolated_env(home)
    env["ROTTWEILER_DRIVER_READY_MARKER"] = DRIVER_READY_MARKER.decode("ascii")
    process = spawn_pty(
        rw,
        env,
        workspace,
        ["--dangerously-trust"],
    )
    closed_normally = False
    try:
        ready = read_until(process, DRIVER_READY_MARKER, timeout=20, phase="driver_ready")
        if FIRST_PAINT_MARKER not in ready:
            raise RuntimeError("supervised TUI became driver-ready without first paint")
        first_tui = wait_for_tui_child(process.pid, tui)
        os.write(process.fd, PROMPT_MARKER.encode())
        # Real echoed input is the focus assertion; driver readiness alone does
        # not prove that a late onboarding modal left the composer in control.
        read_until(process, PROMPT_MARKER.encode(), timeout=8, phase="initial_input_echo")
        os.write(process.fd, TERMINAL_SUBMIT)
        first_transcript = read_until(
            process, RESPONSE_MARKER.encode(), timeout=8, phase="initial_turn_completion"
        )

        os.kill(first_tui, signal.SIGKILL)
        second_tui = wait_for_tui_child(process.pid, tui, exclude=first_tui)
        if second_tui == first_tui:
            raise RuntimeError("supervisor did not replace the SIGKILLed TUI process")
        # A single coalesced replay frame may repaint terminal rows in either
        # order. Wait for both durable rows instead of stopping as soon as the
        # lower assistant row appears and racing the user-row repaint.
        rebuilt = read_until_all(
            process,
            (PROMPT_MARKER.encode(), RESPONSE_MARKER.encode()),
            timeout=8,
            phase="supervised_replay",
        )
        print(
            "M4 supervisor reattach: actual compiled TUI was SIGKILLed and the replacement "
            "re-rendered the complete durable prompt/response transcript"
        )
        owned_children = descendant_pids(process.pid)
        os.write(process.fd, b"\x03")
        try:
            wait_status = wait_for_pty_exit(process, timeout=8)
        except TimeoutError:
            raise RuntimeError("normal TUI Ctrl-C did not stop the installed-bundle supervisor")

        exit_code = os.waitstatus_to_exitcode(wait_status)
        if exit_code != 0:
            raise RuntimeError(
                f"normal TUI Ctrl-C exited the installed-bundle supervisor with {exit_code}"
            )
        cleanup_deadline = time.monotonic() + 5
        while time.monotonic() < cleanup_deadline:
            live_children = [pid for pid in owned_children if process_exists(pid)]
            runtime_leaves = list(runtime_root.glob("engine-*"))
            if not live_children and not runtime_leaves:
                break
            time.sleep(0.01)
        else:
            raise RuntimeError(
                "installed-bundle close leaked supervised children or owned runtime leaves: "
                f"children={live_children!r} runtime={runtime_leaves!r}"
            )
        closed_normally = True
        print(
            "M4 installed-bundle lifecycle: colocated rw/TUI/native resolved without an "
            "override; normal Ctrl-C reaped supervisor children and private runtime leaves"
        )
    finally:
        if not closed_normally:
            terminate_process_tree(process.pid)
        with contextlib.suppress(OSError):
            os.close(process.fd)


def supervisor_parent_death_gate(
    rw: pathlib.Path,
    tui: pathlib.Path,
    root: pathlib.Path,
    workspace: pathlib.Path,
    port: int,
) -> None:
    home = root / "supervisor-parent-death-home"
    write_config(home, port)
    runtime_root = home / "run"
    env = isolated_env(home)
    env["ROTTWEILER_DRIVER_READY_MARKER"] = DRIVER_READY_MARKER.decode("ascii")
    process: PtyProcess | None = spawn_wrapped_pty(
        rw,
        env,
        workspace,
        ["--dangerously-trust"],
    )
    try:
        supervisor = wait_for_supervisor_child(process.pid, rw)
        read_until(process, DRIVER_READY_MARKER, timeout=20)
        engine = wait_for_engine_child(supervisor, rw)
        tui_child = wait_for_tui_child(supervisor, tui)
        os.kill(supervisor, signal.SIGKILL)

        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            live = [pid for pid in (supervisor, engine, tui_child) if process_exists(pid)]
            if not live:
                break
            time.sleep(0.01)
        else:
            raise RuntimeError(
                "SIGKILLed supervisor left parent-watched children alive: "
                f"pids={live!r}"
            )
        stop_pty(process)
        process = None

        # Launching the same workspace must succeed without deleting a lock or
        # runtime directory by hand. The execution lease is a kernel flock, so
        # the meaningful recovery proof is that the orphaned engine released it.
        process = spawn_pty(rw, env, workspace, ["--dangerously-trust"])
        read_until(process, DRIVER_READY_MARKER, timeout=20)
        owned_children = descendant_pids(process.pid)
        terminate_process_tree(process.pid)
        with contextlib.suppress(OSError):
            os.close(process.fd)
        process = None

        cleanup_deadline = time.monotonic() + 5
        while time.monotonic() < cleanup_deadline:
            live = [pid for pid in owned_children if process_exists(pid)]
            live_runtime_descriptors = []
            for descriptor in runtime_root.glob("engine-*/runtime.json"):
                try:
                    runtime_pid = int(json.loads(descriptor.read_text(encoding="utf-8"))["pid"])
                except (OSError, ValueError, KeyError, json.JSONDecodeError):
                    continue
                if process_exists(runtime_pid):
                    live_runtime_descriptors.append(descriptor)
            if not live and not live_runtime_descriptors:
                break
            time.sleep(0.01)
        else:
            raise RuntimeError(
                "parent-death recovery leaked children or a live runtime descriptor: "
                f"children={live!r} runtime={live_runtime_descriptors!r}"
            )
        print(
            "M4 supervisor parent death: SIGKILL reaped the engine and TUI; the same "
            "workspace relaunched without manual lease recovery"
        )
    finally:
        if process is not None:
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
    # See supervisor_reattach_gate: trust is explicit and scoped to this
    # generated fixture process; product defaults remain fail-closed.
    process = spawn_pty(rw, shell_env, workspace, ["--dangerously-trust"])
    try:
        read_until(process, FIRST_PAINT_MARKER, timeout=8)
        read_until(process, DRIVER_READY_MARKER, timeout=8)
        first_engine = wait_for_engine_child(process.pid, rw)
        baseline_requests = origin_request_count()
        os.write(process.fd, f"!{child_script}".encode())
        read_until(process, b"m4-shell-child.py", timeout=3)
        os.write(process.fd, TERMINAL_SUBMIT)
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
        # it to the foreground child's process group. Wait for the durable
        # inactive-shell frame: replay reconciliation keeps OpenTUI suspended
        # while the shell is active, and renderer resume precedes this repaint.
        # The composer's unchanged placeholder is not a PTY readiness signal.
        os.write(process.fd, b"\x03")
        read_until_all(
            process,
            (
                SHELL_INTERRUPT_MARKER.encode(),
                SHELL_EXIT_MARKER.encode(),
            ),
            timeout=5,
        )
        # Normal agent execution resumes only after durable shell completion.
        os.write(process.fd, PROMPT_MARKER.encode())
        read_until(process, PROMPT_MARKER.encode(), timeout=3)
        os.write(process.fd, TERMINAL_SUBMIT)
        read_until(process, RESPONSE_MARKER.encode(), timeout=8)
        # A completed first turn also triggers the product's fast-model title
        # generation. Wait for both intentional provider calls so the gate
        # does not race that asynchronous follow-up.
        expected_requests = baseline_requests + 2
        title_deadline = time.monotonic() + 3
        while (
            origin_request_count() < expected_requests
            and time.monotonic() < title_deadline
        ):
            time.sleep(0.01)
        resumed_requests = origin_request_count()
        if resumed_requests != expected_requests:
            raise RuntimeError(
                "agent turn and title generation did not run exactly once after shell "
                f"completion: expected {expected_requests} origin requests, got "
                f"{resumed_requests}"
            )

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
    event_logs = session_journals(home / "sessions")
    if len(event_logs) != 1:
        raise RuntimeError(f"expected one durable session event log, found {event_logs!r}")
    events: list[dict[str, object]] = []
    for envelope in observed_envelopes(event_logs[0]):
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
        f"export M4_FIXTURE_API_KEY={SHELL_SECRET_VALUE}\n"
        f"exec {rw} \"$@\"\n",
        encoding="utf-8",
    )
    wrapper.chmod(0o700)
    env = isolated_env(home, tui)
    env.update(
        {
            "ROTTWEILER_REMOTE_RW": str(wrapper),
            "ROTTWEILER_SSH_BIN": "/usr/bin/ssh",
            "ROTTWEILER_DRIVER_READY_MARKER": DRIVER_READY_MARKER.decode("ascii"),
        }
    )
    local = spawn_pty(
        rw,
        env,
        workspace,
        ["--dangerously-trust", "--permission-mode", "strict"],
    )
    try:
        local_ready = read_until(local, DRIVER_READY_MARKER, timeout=20)
        if FIRST_PAINT_MARKER not in local_ready:
            raise RuntimeError("local TUI became driver-ready without rendering its first paint")
        os.write(local.fd, PROMPT_MARKER.encode())
        local_capture = read_until(local, PROMPT_MARKER.encode(), timeout=3)
        os.write(local.fd, TERMINAL_SUBMIT)
        local_capture += read_until(local, RESPONSE_MARKER.encode(), timeout=10)
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
            "--dangerously-trust",
            "--remote",
            host,
            "--permission-mode",
            "strict",
            "--resume",
            session_id,
        ],
    )
    remote_closed_normally = False
    try:
        remote_ready = read_until(remote, DRIVER_READY_MARKER, timeout=20)
        if FIRST_PAINT_MARKER not in remote_ready:
            raise RuntimeError("remote TUI became driver-ready without rendering its first paint")
        os.write(remote.fd, PROMPT_MARKER.encode())
        remote_capture = read_until(remote, PROMPT_MARKER.encode(), timeout=3)
        os.write(remote.fd, TERMINAL_SUBMIT)
        remote_capture += read_until(remote, RESPONSE_MARKER.encode(), timeout=10)
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
        descriptor, remote_engine_pid = wait_for_detached_remote(session_id)
        os.write(remote.fd, b"\x03")
        exit_code = os.waitstatus_to_exitcode(wait_pid(remote.pid, 8))
        if exit_code != 0:
            raise RuntimeError(f"attached remote close exited with {exit_code}")
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if not process_exists(remote_engine_pid) and not descriptor.parent.exists():
                break
            time.sleep(0.01)
        else:
            raise RuntimeError(
                "normal remote close leaked its engine or runtime directory: "
                f"pid={remote_engine_pid} runtime={descriptor.parent}"
            )
        remote_closed_normally = True
        print(
            "M4 SSH lifecycle: normal attached Ctrl-C stopped the local TUI, tunnel, "
            "remote engine, and owned remote runtime directory"
        )
    finally:
        if not remote_closed_normally:
            terminate_process_tree(remote.pid)
        with contextlib.suppress(OSError):
            os.close(remote.fd)
        if not remote_closed_normally:
            cleanup_detached_remote(session_id)

    term_session_id = "m4-ssh-loopback-sigterm-gate"
    terminated = spawn_pty(
        rw,
        env,
        workspace,
        [
            "--dangerously-trust",
            "--remote",
            host,
            "--permission-mode",
            "strict",
            "--resume",
            term_session_id,
        ],
    )
    term_closed_normally = False
    try:
        term_ready = read_until(terminated, DRIVER_READY_MARKER, timeout=20)
        if FIRST_PAINT_MARKER not in term_ready:
            raise RuntimeError("SIGTERM remote TUI became driver-ready without first paint")
        descriptor, remote_engine_pid = wait_for_detached_remote(term_session_id)
        os.kill(terminated.pid, signal.SIGTERM)
        exit_code = os.waitstatus_to_exitcode(wait_pid(terminated.pid, 8))
        if exit_code != 0:
            raise RuntimeError(f"attached remote SIGTERM exited with {exit_code}")
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if not process_exists(remote_engine_pid) and not descriptor.parent.exists():
                break
            time.sleep(0.01)
        else:
            raise RuntimeError(
                "SIGTERM remote close leaked its engine or runtime directory: "
                f"pid={remote_engine_pid} runtime={descriptor.parent}"
            )
        term_closed_normally = True
        print(
            "M4 SSH lifecycle: SIGTERM unwound the local TUI, tunnel, owned remote "
            "engine, and runtime directory"
        )
    finally:
        if not term_closed_normally:
            terminate_process_tree(terminated.pid)
        with contextlib.suppress(OSError):
            os.close(terminated.fd)
        if not term_closed_normally:
            cleanup_detached_remote(term_session_id)


def require_visible_markers(captured: bytes) -> None:
    prompt = captured.find(PROMPT_MARKER.encode())
    response = captured.find(RESPONSE_MARKER.encode(), max(0, prompt))
    if prompt < 0 or response < 0 or response < prompt:
        raise RuntimeError("TUI capture omitted the ordered prompt/response transcript")


def canonical_durable_transcript(
    home: pathlib.Path, session_id: str | None = None
) -> bytes:
    if session_id is None:
        event_logs = session_journals(home / "sessions")
        if len(event_logs) != 1:
            raise RuntimeError(
                f"expected one local durable transcript, found {event_logs!r}"
            )
        event_log = event_logs[0]
    else:
        event_log = home / "sessions" / session_id / "journal"
        if not event_log.is_dir():
            raise RuntimeError(f"remote durable transcript is missing: {event_log}")

    turns: list[dict[str, object]] = []
    for envelope in observed_envelopes(event_log):
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


def wait_for_detached_remote(
    session_id: str, timeout: float = 5.0
) -> tuple[pathlib.Path, int]:
    root = pathlib.Path(f"/tmp/rottweiler-{os.geteuid()}")
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for descriptor in root.glob("engine-*/runtime.json"):
            try:
                value = json.loads(descriptor.read_text(encoding="utf-8"))
                if value.get("session_id") != session_id:
                    continue
                pid = int(value["pid"])
            except (OSError, ValueError, KeyError, json.JSONDecodeError):
                continue
            return descriptor, pid
        time.sleep(0.01)
    raise RuntimeError(f"detached remote runtime did not appear for {session_id}")


def cleanup_detached_remote(session_id: str) -> None:
    try:
        descriptor, pid = wait_for_detached_remote(session_id, timeout=0.1)
    except RuntimeError:
        return
    directory = descriptor.parent
    with contextlib.suppress(ProcessLookupError):
        os.kill(pid, signal.SIGTERM)
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
    parser.add_argument("--installed-first-samples", type=int, default=3)
    parser.add_argument("--skip-performance", action="store_true")
    parser.add_argument("--skip-supervisor", action="store_true")
    parser.add_argument("--skip-shell", action="store_true")
    parser.add_argument("--ssh-loopback", metavar="HOST")
    parser.add_argument("--metrics-json", type=pathlib.Path)
    parser.add_argument("--evidence-json", type=pathlib.Path)
    return parser.parse_args()


def opentui_native_library_name() -> str:
    if sys.platform == "darwin":
        return "libopentui.dylib"
    if sys.platform == "win32":
        return "opentui.dll"
    return "libopentui.so"


def run_gate(args: argparse.Namespace, evidence: GateEvidence) -> int:
    if args.samples < 100 and not args.skip_performance:
        raise RuntimeError("p99 release gate requires at least 100 samples")
    if args.installed_first_samples < 3 and not args.skip_performance:
        raise RuntimeError("installed first-launch gate requires at least 3 samples")
    if args.metrics_json is not None and args.skip_performance:
        raise RuntimeError("metric output requires the complete M4 performance gate")
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
    metrics: dict[str, int] = {}
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
                evidence.update(phase="installed_first_launch")
                metrics.update(
                    installed_first_launch_gate(
                        source_rw,
                        source_tui,
                        source_tui_native,
                        root,
                        workspace,
                        port,
                        args.installed_first_samples,
                        evidence,
                    )
                )
                evidence.update(phase="startup", metrics=metrics)
                metrics.update(performance_gate(rw, tui, root, workspace, port, args.samples, evidence))
                evidence.update(phase="uds_queries", metrics=metrics)
                metrics.update(socket_latency_gate(rw, root, workspace, port, args.samples, evidence))
            if not args.skip_supervisor:
                evidence.update(phase="provider_discovery", metrics=metrics)
                model_discovery_gate(rw, root, workspace, port)
                evidence.update(phase="supervisor_reattach", metrics=metrics)
                supervisor_reattach_gate(rw, tui, root, workspace, port)
                evidence.update(phase="supervisor_parent_death")
                supervisor_parent_death_gate(rw, tui, root, workspace, port)
            if not args.skip_shell:
                evidence.update(phase="shell_handover")
                shell_handover_gate(rw, tui, root, workspace, port)
            if args.ssh_loopback is not None:
                evidence.update(phase="ssh_loopback")
                ssh_loopback_gate(rw, tui, root, workspace, port, args.ssh_loopback)
    if args.metrics_json is not None:
        metrics["tui_bundle_bytes"] = source_tui.stat().st_size + source_tui_native.stat().st_size
        args.metrics_json.parent.mkdir(parents=True, exist_ok=True)
        temporary = args.metrics_json.with_name(f".{args.metrics_json.name}.tmp")
        temporary.write_text(
            json.dumps({"schema_version": 1, "metrics": metrics}, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        temporary.replace(args.metrics_json)
    return 0


def main() -> int:
    args = parse_args()
    output = args.evidence_json
    if output is None and args.metrics_json is not None:
        output = args.metrics_json.with_name(f"{args.metrics_json.stem}-evidence.json")
    evidence = GateEvidence(output)
    try:
        evidence.update()
        result = run_gate(args, evidence)
        evidence.update(status="pass", phase="complete")
        return result
    except BaseException as error:
        try:
            evidence.failure(error)
        except OSError:
            pass
        raise


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:  # noqa: BLE001 - gate prints one actionable failure
        message = str(error).replace(SHELL_SECRET_VALUE, "[REDACTED]")[-8_000:]
        print(f"M4 release gate failed: {message}", file=sys.stderr)
        raise SystemExit(1) from error
