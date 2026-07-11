#!/usr/bin/env python3
"""Honest offline M8 release cold-start to prompt-ready acceptance gate."""

from __future__ import annotations

import argparse
import contextlib
import json
import math
import os
import pathlib
import pty
import re
import select
import shutil
import signal
import statistics
import subprocess
import tempfile
import time


PROMPT_READY_MARKER = b"rw_perf_prompt_ready=1\n"
FINGERPRINT = re.compile(rb"/mcp approve ([A-Za-z0-9_.-]+) ([0-9a-f]{64})")


def isolated_env(home: pathlib.Path, temporary: pathlib.Path) -> dict[str, str]:
    return {
        "HOME": str(home),
        "ROTTWEILER_HOME": str(home),
        "ROTTWEILER_CREDENTIAL_BACKEND": "file",
        "TMPDIR": str(temporary),
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
    }


def grant_exact_project_trust(
    rw: pathlib.Path,
    workspace: pathlib.Path,
    env: dict[str, str],
) -> None:
    pid, descriptor = pty.fork()
    if pid == 0:
        os.chdir(workspace)
        os.execve(str(rw), [str(rw), "trust", "grant"], env)
    captured = bytearray()
    try:
        deadline = time.monotonic() + 10
        prompted = False
        while time.monotonic() < deadline:
            ready, _, _ = select.select([descriptor], [], [], 0.05)
            if not ready:
                continue
            try:
                chunk = os.read(descriptor, 65536)
            except OSError:
                break
            if not chunk:
                break
            captured.extend(chunk)
            if not prompted and b"Trust this exact executable inventory?" in captured:
                os.write(descriptor, b"y\n")
                prompted = True
    finally:
        with contextlib.suppress(OSError):
            os.close(descriptor)
    status: int | None = None
    reap_deadline = time.monotonic() + 2
    while time.monotonic() < reap_deadline:
        found, candidate = os.waitpid(pid, os.WNOHANG)
        if found == pid:
            status = candidate
            break
        time.sleep(0.01)
    if status is None:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(pid, signal.SIGTERM)
        time.sleep(0.1)
        found, candidate = os.waitpid(pid, os.WNOHANG)
        if found == pid:
            status = candidate
        else:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(pid, signal.SIGKILL)
            _, status = os.waitpid(pid, 0)
    exit_code = os.waitstatus_to_exitcode(status)
    if not prompted:
        raise RuntimeError(f"folder trust prompt was not shown: {captured[-2000:]!r}")
    if exit_code != 0:
        raise RuntimeError(
            f"folder trust grant failed with exit code {exit_code}: {captured[-2000:]!r}"
        )
    status_run = subprocess.run(
        [str(rw), "trust", "status"],
        cwd=workspace,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=10,
        check=False,
    )
    if status_run.returncode != 0 or b"state: Trusted" not in status_run.stdout:
        raise RuntimeError(
            "persisted exact folder trust was not rediscovered: "
            f"stdout={status_run.stdout!r} stderr={status_run.stderr!r}"
        )


def run_command(
    rw: pathlib.Path,
    workspace: pathlib.Path,
    env: dict[str, str],
    provider_script: pathlib.Path,
    command: str,
) -> subprocess.CompletedProcess[bytes]:
    run = subprocess.run(
        [
            str(rw),
            "-p",
            command,
            "--permission-mode",
            "strict",
            "--in-memory-replay-script",
            str(provider_script),
            "--output-format",
            "text",
            "--perf-markers",
        ],
        cwd=workspace,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=15,
        check=False,
    )
    if run.returncode != 0:
        raise RuntimeError(
            f"rw command {command!r} failed: stdout={run.stdout!r} stderr={run.stderr!r}"
        )
    return run


def approve_exact_mcp_configs(
    rw: pathlib.Path,
    workspace: pathlib.Path,
    env: dict[str, str],
    provider_script: pathlib.Path,
    server_names: list[str],
) -> None:
    expected: dict[str, str] = {}
    for server in server_names:
        summary = run_command(rw, workspace, env, provider_script, f"/mcp approve {server}")
        match = FINGERPRINT.search(summary.stdout)
        if match is None or match.group(1).decode("ascii") != server:
            raise RuntimeError(
                f"MCP {server} did not render its exact approval fingerprint: {summary.stdout!r}"
            )
        fingerprint = match.group(2).decode("ascii")
        expected[server] = fingerprint
        confirmation = run_command(
            rw,
            workspace,
            env,
            provider_script,
            f"/mcp approve {server} {fingerprint}",
        )
        if b'"config_approved":true' not in confirmation.stdout:
            raise RuntimeError(
                f"MCP {server} approval was not durably installed: {confirmation.stdout!r}"
            )
    ledger_path = pathlib.Path(env["ROTTWEILER_HOME"]) / "mcp-approvals-v1.json"
    ledger = json.loads(ledger_path.read_text(encoding="utf-8"))
    if ledger != {"version": 1, "approvals": expected}:
        raise RuntimeError(f"approval ledger did not contain exactly three configs: {ledger!r}")


def parse_status(stdout: bytes, server_names: list[str]) -> None:
    statuses: object | None = None
    text = stdout.decode("utf-8", errors="replace")
    decoder = json.JSONDecoder()
    for start, character in enumerate(text):
        if character != "[":
            continue
        with contextlib.suppress(json.JSONDecodeError):
            value, _ = decoder.raw_decode(text[start:])
            if isinstance(value, list) and all(
                isinstance(entry, dict) and isinstance(entry.get("id"), str)
                for entry in value
            ):
                statuses = value
    if not isinstance(statuses, list) or len(statuses) != len(server_names):
        raise RuntimeError(f"/mcp status omitted three real catalogs: {stdout!r}")
    by_name = {entry.get("id"): entry for entry in statuses if isinstance(entry, dict)}
    for server in server_names:
        status = by_name.get(server)
        if status is None or status.get("state") != "ready":
            raise RuntimeError(f"MCP {server} was not ready: {statuses!r}")
        if (
            status.get("tool_count") != 3
            or status.get("resource_count") != 1
            or status.get("prompt_count") != 1
        ):
            raise RuntimeError(f"MCP {server} catalog evidence was incomplete: {status!r}")


def process_table() -> list[tuple[int, int, int, str]]:
    output = subprocess.check_output(
        ["ps", "-axo", "pid=,ppid=,pgid=,command="], text=True
    )
    records: list[tuple[int, int, int, str]] = []
    for line in output.splitlines():
        fields = line.strip().split(maxsplit=3)
        if len(fields) != 4:
            continue
        records.append((int(fields[0]), int(fields[1]), int(fields[2]), fields[3]))
    return records


def descendant_processes(root_pid: int) -> list[tuple[int, int, int, str]]:
    records = process_table()
    children: dict[int, list[tuple[int, int, int, str]]] = {}
    for record in records:
        children.setdefault(record[1], []).append(record)
    pending = list(children.get(root_pid, []))
    descendants: list[tuple[int, int, int, str]] = []
    while pending:
        record = pending.pop()
        descendants.append(record)
        pending.extend(children.get(record[0], []))
    return descendants


def fixture_processes(
    descendants: list[tuple[int, int, int, str]], fixture: pathlib.Path
) -> list[tuple[int, int]]:
    fixtures: list[tuple[int, int]] = []
    expected = fixture.resolve()
    for pid, _, pgid, command in descendants:
        program = command.split(maxsplit=1)[0]
        with contextlib.suppress(OSError):
            if pathlib.Path(program).resolve() == expected:
                fixtures.append((pid, pgid))
    return sorted(fixtures)


def group_members(groups: set[int]) -> list[tuple[int, int]]:
    return sorted((pid, pgid) for pid, _, pgid, _ in process_table() if pgid in groups)


def signal_groups(groups: set[int], signal_number: signal.Signals) -> None:
    own_group = os.getpgrp()
    for group in sorted(groups):
        if group <= 0 or group == own_group:
            continue
        with contextlib.suppress(ProcessLookupError):
            os.killpg(group, signal_number)


def terminate_process_tree(
    process: subprocess.Popen[bytes], captured_child_groups: set[int]
) -> None:
    refreshed = {record[2] for record in descendant_processes(process.pid)}
    child_groups = (captured_child_groups | refreshed) - {process.pid}
    signal_groups(child_groups, signal.SIGTERM)
    signal_groups({process.pid}, signal.SIGTERM)
    with contextlib.suppress(subprocess.TimeoutExpired):
        process.wait(timeout=0.5)
    signal_groups(child_groups, signal.SIGKILL)
    signal_groups({process.pid}, signal.SIGKILL)
    with contextlib.suppress(subprocess.TimeoutExpired):
        process.wait(timeout=2)
    # A helper can fork between the first snapshot and parent termination.
    signal_groups(child_groups, signal.SIGKILL)


def write_terminal_line(descriptor: int, line: str) -> None:
    for byte in line.encode("utf-8"):
        os.write(descriptor, bytes([byte]))
        time.sleep(0.001)
    os.write(descriptor, b"\r")


def append_bounded(buffer: bytearray, chunk: bytes) -> None:
    buffer.extend(chunk)
    limit = 4 * 1024 * 1024
    if len(buffer) > limit:
        del buffer[: len(buffer) - limit]


def one_sample(
    rw: pathlib.Path,
    workspace: pathlib.Path,
    seeded_home: pathlib.Path,
    sample_root: pathlib.Path,
    provider_script: pathlib.Path,
    fixture: pathlib.Path,
    server_names: list[str],
    sample: int,
) -> float:
    # Every process starts from identical persisted trust and approval state;
    # copying this baseline is intentionally outside the measured interval.
    home = sample_root / "home"
    scratch = sample_root / "tmp"
    shutil.copytree(seeded_home, home)
    scratch.mkdir(mode=0o700)
    env = isolated_env(home, scratch)
    command = [
        str(rw),
        "--line",
        "--permission-mode",
        "strict",
        "--in-memory-replay-script",
        str(provider_script),
        "--output-format",
        "text",
        "--perf-markers",
    ]
    terminal_master, terminal_slave = pty.openpty()
    started = time.perf_counter_ns()
    process = subprocess.Popen(
        command,
        cwd=workspace,
        env=env,
        stdin=terminal_slave,
        stdout=terminal_slave,
        stderr=subprocess.PIPE,
        bufsize=0,
        start_new_session=True,
    )
    os.close(terminal_slave)
    assert process.stderr is not None
    stderr_descriptor = process.stderr.fileno()
    captured_stderr = bytearray()
    terminal_output = bytearray()
    prompt_ready_ms: float | None = None
    fixture_records: list[tuple[int, int]] = []
    child_groups: set[int] = set()
    deadline = time.monotonic() + 10
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select(
                [stderr_descriptor, terminal_master], [], [], 0.01
            )
            if not ready:
                if process.poll() is not None:
                    break
                continue
            for descriptor in ready:
                try:
                    chunk = os.read(descriptor, 65536)
                except OSError:
                    chunk = b""
                if descriptor == stderr_descriptor:
                    append_bounded(captured_stderr, chunk)
                else:
                    append_bounded(terminal_output, chunk)
            if (
                PROMPT_READY_MARKER in captured_stderr
                and b"rw> " in terminal_output
            ):
                prompt_ready_ms = (time.perf_counter_ns() - started) / 1_000_000
                descendants = descendant_processes(process.pid)
                fixture_records = fixture_processes(descendants, fixture)
                child_groups = {record[2] for record in descendants} - {process.pid}
                break
        if prompt_ready_ms is None:
            raise RuntimeError(
                f"sample {sample} exited before composition plus line prompt were ready: "
                f"terminal={terminal_output[-1500:]!r} stderr={captured_stderr[-1500:]!r}"
            )
        write_terminal_line(terminal_master, "/mcp status")
        status_deadline = time.monotonic() + 5
        status_ready = False
        while time.monotonic() < status_deadline:
            ready, _, _ = select.select(
                [stderr_descriptor, terminal_master], [], [], 0.01
            )
            for descriptor in ready:
                try:
                    chunk = os.read(descriptor, 65536)
                except OSError:
                    chunk = b""
                if descriptor == stderr_descriptor:
                    append_bounded(captured_stderr, chunk)
                else:
                    append_bounded(terminal_output, chunk)
            with contextlib.suppress(RuntimeError):
                parse_status(bytes(terminal_output), server_names)
                status_ready = True
            if status_ready:
                break
            if process.poll() is not None:
                break
        if not status_ready:
            raise RuntimeError(
                f"sample {sample} did not render /mcp status: {terminal_output[-3000:]!r}"
            )
        parse_status(bytes(terminal_output), server_names)
        exit_deadline = time.monotonic() + 5
        while terminal_output.count(b"rw> ") < 2 and time.monotonic() < exit_deadline:
            ready, _, _ = select.select(
                [stderr_descriptor, terminal_master], [], [], 0.01
            )
            for descriptor in ready:
                try:
                    chunk = os.read(descriptor, 65536)
                except OSError:
                    chunk = b""
                if descriptor == stderr_descriptor:
                    append_bounded(captured_stderr, chunk)
                else:
                    append_bounded(terminal_output, chunk)
        if terminal_output.count(b"rw> ") < 2:
            raise RuntimeError(
                f"sample {sample} line client did not return after /mcp status: "
                f"{terminal_output[-3000:]!r}"
            )
        # Rustyline maps Ctrl-D at an empty prompt to EOF; the production REPL
        # then follows its normal MCP shutdown path.
        os.write(terminal_master, b"\x04")
        shutdown_deadline = time.monotonic() + 10
        while time.monotonic() < shutdown_deadline:
            ready, _, _ = select.select(
                [stderr_descriptor, terminal_master], [], [], 0.01
            )
            for descriptor in ready:
                try:
                    chunk = os.read(descriptor, 65536)
                except OSError:
                    chunk = b""
                if descriptor == stderr_descriptor:
                    append_bounded(captured_stderr, chunk)
                else:
                    append_bounded(terminal_output, chunk)
            if process.poll() is not None:
                break
        process.wait(timeout=1)
    except BaseException:
        terminate_process_tree(process, child_groups)
        raise
    finally:
        with contextlib.suppress(OSError):
            os.close(terminal_master)
    if process.returncode != 0:
        raise RuntimeError(
            f"sample {sample} failed rc={process.returncode}: "
            f"terminal={bytes(terminal_output)!r} stderr={bytes(captured_stderr)!r}"
        )
    fixture_pids = {pid for pid, _ in fixture_records}
    fixture_groups = {group for _, group in fixture_records}
    if len(fixture_pids) != 3 or len(fixture_groups) != 3:
        raise RuntimeError(
            f"sample {sample} did not expose three canonical fixture processes in distinct "
            f"groups at prompt-ready: {fixture_records!r}"
        )
    leaked = group_members(child_groups)
    if leaked:
        signal_groups(child_groups, signal.SIGKILL)
        raise RuntimeError(
            f"sample {sample} did not shutdown/reap complete MCP child groups: {leaked!r}"
        )
    shutil.rmtree(sample_root)
    return prompt_ready_ms


def percentile(values: list[float], quantile: float) -> float:
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * quantile) - 1)]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rw", type=pathlib.Path, required=True)
    parser.add_argument("--fixture", type=pathlib.Path, required=True)
    parser.add_argument("--samples", type=int, default=100)
    parser.add_argument("--functional-only", action="store_true")
    parser.add_argument("--metrics-json", type=pathlib.Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.samples < 100 and not args.functional_only:
        raise RuntimeError("M8 p99 release gate requires at least 100 samples")
    if args.samples < 1:
        raise RuntimeError("M8 gate requires at least one sample")
    if args.metrics_json is not None and args.functional_only:
        raise RuntimeError("metric output requires the complete M8 performance gate")
    source_rw = args.rw.resolve()
    source_fixture = args.fixture.resolve()
    if not source_rw.is_file() or not source_fixture.is_file():
        raise RuntimeError("release rw and rw-mcp-fixture binaries must exist")
    with tempfile.TemporaryDirectory(
        prefix="rw8-", dir=tempfile.gettempdir()
    ) as temporary:
        # `/tmp` is a symlink on macOS. Production protocol launchers reject
        # any symlink provenance, so every path placed into config or argv must
        # use the canonical `/private/tmp` spelling.
        root = pathlib.Path(temporary).resolve()
        root.chmod(0o700)
        artifact_bin = root / "bin"
        artifact_bin.mkdir(mode=0o700)
        rw = artifact_bin / "rw"
        fixture = artifact_bin / "rw-mcp-fixture"
        shutil.copyfile(source_rw, rw)
        shutil.copyfile(source_fixture, fixture)
        rw.chmod(0o700)
        fixture.chmod(0o700)
        workspace = root / "workspace"
        workspace.mkdir(mode=0o700)
        agents = workspace / ".agents"
        agents.mkdir(mode=0o700)
        home = root / "home"
        home.mkdir(mode=0o700)
        scratch = root / "tmp"
        scratch.mkdir(mode=0o700)
        server_names = ["alpha", "bravo", "charlie"]
        config_lines: list[str] = []
        for server in server_names:
            config_lines.extend(
                [
                    f"[servers.{server}]",
                    f'argv = [{json.dumps(str(fixture))}]',
                    "enabled = true",
                    "defer_tools = true",
                    "",
                ]
            )
        (agents / "mcp.toml").write_text("\n".join(config_lines), encoding="utf-8")
        provider_script = root / "provider.json"
        provider_script.write_text(
            '[[{"type":"text_delta","text":"unused"},{"type":"finished","reason":"stop"}]]',
            encoding="utf-8",
        )
        env = isolated_env(home, scratch)
        grant_exact_project_trust(rw, workspace, env)
        approve_exact_mcp_configs(
            rw, workspace, env, provider_script, server_names
        )
        # Seed only the fixed persisted security state. Session/index/history
        # artifacts from approval setup are not part of any startup sample.
        for volatile in [
            home / "sessions",
            home / "index.sqlite",
            home / "index.sqlite-wal",
            home / "index.sqlite-shm",
            home / "history.txt",
        ]:
            if volatile.is_dir():
                shutil.rmtree(volatile)
            else:
                with contextlib.suppress(FileNotFoundError):
                    volatile.unlink()
        samples_root = root / "samples"
        samples_root.mkdir(mode=0o700)
        # Five warm-cache policy/executable warmups, each still a fresh process
        # with a fresh copy of the exact seeded HOME.
        if not args.functional_only:
            for sample in range(-5, 0):
                one_sample(
                    rw,
                    workspace,
                    home,
                    samples_root / f"warmup-{sample + 5}",
                    provider_script,
                    fixture,
                    server_names,
                    sample,
                )
        measurements = [
            one_sample(
                rw,
                workspace,
                home,
                samples_root / f"sample-{sample}",
                provider_script,
                fixture,
                server_names,
                sample,
            )
            for sample in range(args.samples)
        ]
        p99 = percentile(measurements, 0.99)
        print(
            "M8 warm-cache fresh-process startup: "
            f"samples={len(measurements)}; "
            f"three_stdio_mcp_prompt_ready_ms p50={statistics.median(measurements):.3f} "
            f"p99={p99:.3f} max={max(measurements):.3f}"
        )
        if not args.functional_only and p99 >= 250:
            raise RuntimeError(
                f"three-server cold-start to prompt-ready p99 {p99:.3f}ms exceeds 250ms"
            )
        if args.metrics_json is not None:
            args.metrics_json.parent.mkdir(parents=True, exist_ok=True)
            temporary = args.metrics_json.with_name(f".{args.metrics_json.name}.tmp")
            temporary.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "metrics": {"mcp_prompt_ready_p99_us": math.ceil(p99 * 1000)},
                    },
                    sort_keys=True,
                )
                + "\n",
                encoding="utf-8",
            )
            temporary.replace(args.metrics_json)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:  # noqa: BLE001 - gate prints one actionable failure
        print(f"M8 release gate failed: {error}", file=os.sys.stderr)
        raise SystemExit(1) from error
