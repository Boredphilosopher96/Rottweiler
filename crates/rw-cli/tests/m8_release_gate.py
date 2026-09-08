#!/usr/bin/env python3
"""Honest offline M8 release cold-start to prompt-ready acceptance gate."""

from __future__ import annotations

import argparse
import contextlib
import ctypes
import hashlib
import json
import math
import os
import pathlib
import re
import select
import shutil
import statistics
import stat
import sys
import subprocess
import tempfile
import time


REPO = pathlib.Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO / "scripts"))
from perf_process import run_sample, check_sample_cancellation, delegated_success_scope
from perf_process_wait import observe_exit, require_group_disappearance
from perf_process_scope import UnsettledScope
from perf_scratch import retained_scratch
from m8_process import Terminal, append_bounded
import m8_inputs


PROMPT_READY_MARKER = b"rw_perf_prompt_ready=1\n"
STATUS_READY_TIMEOUT_SECONDS = 10.0
FINGERPRINT = re.compile(rb"/mcp approve ([A-Za-z0-9_.-]+) ([0-9a-f]{64})")
ANSI_ESCAPE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
PROJECT_TRUST_PROMPT = "Trust this exact project extension inventory? [y/N] "
TRUST_INVENTORY_ROW = re.compile(
    r"^  (?P<path>\S+) \[(?P<kind>[a-z_]+)\] "
    r"(?P<bytes>\d+) bytes hash (?P<hash>[0-9a-f]{64})$",
    re.MULTILINE,
)
MCP_STATUS_ROW = re.compile(
    r"^- (?P<id>[A-Za-z0-9_.-]+) · (?P<state>.+?) · "
    r"(?P<tools>\d+) tools · (?P<resources>\d+) resources · "
    r"(?P<prompts>\d+) prompts$",
    re.MULTILINE,
)


def normalized_terminal_text(output: bytes) -> str:
    return ANSI_ESCAPE.sub("", output.decode("utf-8", errors="replace")).replace(
        "\r", ""
    )


def validate_project_trust_inventory(
    output: bytes,
    workspace: pathlib.Path,
    inventory_file: pathlib.Path,
    *,
    expected_state: str,
    expected_hash: str | None = None,
    require_initial_addition: bool = False,
    require_prompt: bool = False,
) -> str:
    text = normalized_terminal_text(output)
    workspace_line = f"workspace: {workspace.resolve()}"
    state_line = f"state: {expected_state}"
    lines = text.splitlines()
    try:
        assessment_start = lines.index(workspace_line)
    except ValueError as error:
        raise RuntimeError(
            f"trust assessment did not identify the exact workspace: {text[-2000:]!r}"
        ) from error
    if lines[assessment_start + 1 : assessment_start + 3] != [
        state_line,
        "project extension inventory:",
    ]:
        raise RuntimeError(
            "trust assessment did not report the expected state and inventory header: "
            f"{text[-2000:]!r}"
        )
    rows = list(TRUST_INVENTORY_ROW.finditer(text))
    inventory_line = (
        lines[assessment_start + 3]
        if len(lines) > assessment_start + 3
        else ""
    )
    if len(rows) != 1 or rows[0].group(0) != inventory_line:
        raise RuntimeError(
            "trust assessment did not contain exactly one well-formed inventory row: "
            f"{text[-2000:]!r}"
        )
    row = rows[0].groupdict()
    expected_path = inventory_file.relative_to(workspace).as_posix()
    expected_bytes = inventory_file.stat().st_size
    if (
        row["path"] != expected_path
        or row["kind"] != "mcp"
        or int(row["bytes"]) != expected_bytes
    ):
        raise RuntimeError(
            "trust assessment described the wrong project extension: "
            f"expected={expected_path!r}/mcp/{expected_bytes} actual={row!r}"
        )
    if require_initial_addition:
        expected_changes = [
            "changes since last trust:",
            f"  + {expected_path}",
        ]
        if lines[assessment_start + 4 : assessment_start + 6] != expected_changes:
            raise RuntimeError(
                "initial trust assessment did not report the exact inventory addition: "
                f"{text[-2000:]!r}"
            )
    elif "changes since last trust:" in lines:
        raise RuntimeError(
            f"trust assessment unexpectedly reported inventory changes: {text[-2000:]!r}"
        )
    if expected_hash is not None and row["hash"] != expected_hash:
        raise RuntimeError(
            "persisted trust inventory hash differs from the approved challenge: "
            f"expected={expected_hash!r} actual={row['hash']!r}"
        )
    if require_prompt:
        prompt_index = assessment_start + (6 if require_initial_addition else 4)
        if lines[prompt_index : prompt_index + 1] != [PROJECT_TRUST_PROMPT]:
            raise RuntimeError(
                "exact project extension inventory trust challenge was not shown "
                "immediately after the assessment: "
                f"{text[-2000:]!r}"
            )
    return row["hash"]


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
    inventory_file: pathlib.Path,
) -> None:
    terminal = Terminal([str(rw), "trust", "grant"], cwd=workspace, env=env)
    captured = bytearray()
    prompted_hash = None
    try:
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            check_sample_cancellation()
            output, errors = terminal.read(.05)
            append_bounded(captured, output)
            append_bounded(captured, errors)
            if prompted_hash is None and PROJECT_TRUST_PROMPT.encode() in captured:
                prompted_hash = validate_project_trust_inventory(
                    bytes(captured), workspace, inventory_file, expected_state="Untrusted",
                    require_initial_addition=True, require_prompt=True,
                )
                terminal.write(b"y\n", deadline=deadline)
            if terminal.observe_exit() is not None:
                break
        status = terminal.observe_exit()
        if prompted_hash is None:
            raise RuntimeError(f"exact project trust challenge was not shown: {captured[-2000:]!r}")
        if status != 0:
            raise RuntimeError(f"project trust grant failed or timed out: {status}: {captured[-2000:]!r}")
    finally:
        terminal.close()
    status_run = run_sample([str(rw), "trust", "status"], cwd=workspace, env=env, timeout=10,
                            output_limit=4 * 1024 * 1024)
    if status_run.returncode != 0:
        raise RuntimeError(f"persisted project trust could not be read: {status_run.stderr[-2000:]!r}")
    validate_project_trust_inventory(
        status_run.stdout,
        workspace,
        inventory_file,
        expected_state="Trusted",
        expected_hash=prompted_hash,
    )


def run_command(
    rw: pathlib.Path,
    workspace: pathlib.Path,
    env: dict[str, str],
    provider_script: pathlib.Path,
    command: str,
) -> subprocess.CompletedProcess[bytes]:
    run = run_sample(
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
        timeout=15,
        output_limit=4 * 1024 * 1024,
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
        human_confirmation = (
            f"MCP server {server} is approved.\nConfiguration: new approval saved\n"
        ).encode("utf-8")
        if human_confirmation not in confirmation.stdout:
            raise RuntimeError(
                f"MCP {server} did not confirm its saved approval: {confirmation.stdout!r}"
            )
        ledger_path = pathlib.Path(env["ROTTWEILER_HOME"]) / "mcp-approvals-v1.json"
        ledger = json.loads(ledger_path.read_text(encoding="utf-8"))
        if ledger.get("approvals", {}).get(server) != fingerprint:
            raise RuntimeError(
                f"MCP {server} approval was not durably installed: {ledger!r}"
            )
    ledger_path = pathlib.Path(env["ROTTWEILER_HOME"]) / "mcp-approvals-v1.json"
    ledger = json.loads(ledger_path.read_text(encoding="utf-8"))
    if ledger != {"version": 1, "approvals": expected}:
        raise RuntimeError(f"approval ledger did not contain exactly three configs: {ledger!r}")


def parse_status(
    stdout: bytes,
    server_names: list[str],
    *,
    expected_ready: set[str],
) -> None:
    text = ANSI_ESCAPE.sub("", stdout.decode("utf-8", errors="replace")).replace(
        "\r", ""
    )
    matches = list(MCP_STATUS_ROW.finditer(text))
    latest = matches[-len(server_names) :]
    statuses = {match.group("id"): match.groupdict() for match in latest}
    if len(latest) != len(server_names) or set(statuses) != set(server_names):
        raise RuntimeError(f"/mcp status omitted three real catalogs: {stdout!r}")
    for server in server_names:
        status = statuses[server]
        expected_state = "ready" if server in expected_ready else "disabled"
        expected_catalog = (3, 1, 1) if server in expected_ready else (0, 0, 0)
        if status["state"] != expected_state:
            raise RuntimeError(
                f"MCP {server} was not {expected_state}: {statuses!r}"
            )
        catalog = (
            int(status["tools"]),
            int(status["resources"]),
            int(status["prompts"]),
        )
        if catalog != expected_catalog:
            raise RuntimeError(f"MCP {server} catalog evidence was incomplete: {status!r}")


def process_table() -> list[tuple[int, int, int, str]]:
    observed = run_sample(["/bin/ps", "-axo", "pid=,ppid=,pgid=,command="],
                          cwd=REPO, env=dict(os.environ), timeout=5, output_limit=4 * 1024 * 1024)
    if observed.returncode != 0:
        raise RuntimeError("M8 process observation failed")
    output = observed.stdout.decode("utf-8", errors="strict")
    if not output.strip():
        raise RuntimeError("M8 process observation was empty")
    records: list[tuple[int, int, int, str]] = []
    for line in output.splitlines():
        fields = line.strip().split(maxsplit=3)
        if len(fields) != 4:
            raise RuntimeError("M8 process observation was malformed")
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


def process_image_path(pid: int) -> pathlib.Path:
    """Ask the kernel for the executing image, independent of its argv spelling."""
    if sys.platform == "linux":
        return pathlib.Path(f"/proc/{pid}/exe")
    if sys.platform == "darwin":
        library = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
        library.proc_pidpath.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]
        library.proc_pidpath.restype = ctypes.c_int
        buffer = ctypes.create_string_buffer(4096)  # PROC_PIDPATHINFO_MAXSIZE
        if library.proc_pidpath(pid, buffer, len(buffer)) <= 0:
            cause = ctypes.get_errno()
            raise OSError(cause, f"cannot identify running image for PID {pid}")
        return pathlib.Path(os.fsdecode(buffer.value))
    raise RuntimeError(f"unsupported M8 process-image platform: {sys.platform}")


def image_identity(path: pathlib.Path, expected_bytes: int) -> tuple[tuple[int, ...], str] | None:
    """Hash a bounded, stable descriptor; private copies and sealed memfds are valid."""
    with path.open("rb") as source:
        before = os.fstat(source.fileno())
        if not stat.S_ISREG(before.st_mode) or before.st_size != expected_bytes:
            return None
        digest = hashlib.sha256()
        copied = 0
        while chunk := source.read(64 * 1024):
            copied += len(chunk)
            if copied > expected_bytes:
                raise RuntimeError("running image grew during identity verification")
            digest.update(chunk)
        after = os.fstat(source.fileno())
    fields = ("st_dev", "st_ino", "st_size", "st_mtime_ns", "st_ctime_ns")
    identity = tuple(getattr(before, field) for field in fields)
    if copied != expected_bytes or identity != tuple(getattr(after, field) for field in fields):
        raise RuntimeError("running image changed during identity verification")
    return identity, digest.hexdigest()


def fixture_processes(
    descendants: list[tuple[int, int, int, str]], fixture: pathlib.Path
) -> list[tuple[int, int]]:
    fixtures: list[tuple[int, int]] = []
    expected_bytes = fixture.stat().st_size
    expected = image_identity(fixture, expected_bytes)
    if expected is None:
        raise RuntimeError("approved fixture is not a regular executable image")
    for pid, _, pgid, _ in descendants:
        try:
            image = process_image_path(pid)
            identity = image_identity(image, expected_bytes)
            if identity is None or identity[1] != expected[1]:
                continue
            # The process must still execute that descriptor in the captured group.
            current = process_image_path(pid).stat()
            fields = ("st_dev", "st_ino", "st_size", "st_mtime_ns", "st_ctime_ns")
            if tuple(getattr(current, field) for field in fields) != identity[0]:
                raise RuntimeError(f"fixture PID {pid} changed its running image")
            if os.getpgid(pid) != pgid:
                raise RuntimeError(f"fixture PID {pid} changed its process group")
            fixtures.append((pid, pgid))
        except (FileNotFoundError, ProcessLookupError):
            # A descendant may naturally retire while its snapshot is inspected.
            continue
    return sorted(fixtures)


def group_members(groups: set[int]) -> list[tuple[int, int]]:
    return sorted((pid, pgid) for pid, _, pgid, _ in process_table() if pgid in groups)


def write_terminal_line(terminal: Terminal, line: str) -> None:
    deadline = time.monotonic() + STATUS_READY_TIMEOUT_SECONDS
    for byte in line.encode("utf-8"):
        terminal.write(bytes([byte]), deadline=deadline)
        time.sleep(0.001)
    terminal.write(b"\r", deadline=deadline)


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
        "--permission-mode",
        "strict",
        "--in-memory-replay-script",
        str(provider_script),
        "--output-format",
        "text",
        "--perf-markers",
    ]
    terminal = Terminal(command, cwd=workspace, env=env)
    started = terminal.spawn_started_ns
    process = terminal.owner.process
    terminal_master = terminal.master
    assert process.stderr is not None
    stderr_descriptor = process.stderr.fileno()
    captured_stderr = bytearray()
    terminal_output = bytearray()
    prompt_ready_ms: float | None = None
    startup_fixture_records: list[tuple[int, int]] = []
    fixture_records: list[tuple[int, int]] = []
    child_groups: set[int] = set()
    deadline = time.monotonic() + 10
    try:
        while time.monotonic() < deadline:
            check_sample_cancellation()
            ready, _, _ = select.select(
                [stderr_descriptor, terminal_master], [], [], 0.01
            )
            if not ready:
                if observe_exit(process.pid) is not None:
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
                startup_fixture_records = fixture_processes(descendants, fixture)
                break
        if prompt_ready_ms is None:
            raise RuntimeError(
                f"sample {sample} exited before composition plus line prompt were ready: "
                f"terminal={terminal_output[-1500:]!r} stderr={captured_stderr[-1500:]!r}"
            )
        write_terminal_line(terminal, "/mcp status")
        # Status rendering is a functional assertion after the measured
        # prompt-ready interval. Protected runners can briefly deschedule the
        # PTY consumer while the command is already queued, so give that
        # unmeasured hand-off the same tail allowance as MCP activation.
        status_deadline = time.monotonic() + STATUS_READY_TIMEOUT_SECONDS
        status_ready = False
        while time.monotonic() < status_deadline:
            check_sample_cancellation()
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
                parse_status(
                    bytes(terminal_output),
                    server_names,
                    expected_ready=set(),
                )
                status_ready = True
            if status_ready:
                break
            if observe_exit(process.pid) is not None:
                break
        if not status_ready:
            raise RuntimeError(
                f"sample {sample} did not render /mcp status: {terminal_output[-3000:]!r}"
            )
        parse_status(
            bytes(terminal_output),
            server_names,
            expected_ready=set(),
        )
        if startup_fixture_records:
            raise RuntimeError(
                f"sample {sample} eagerly started MCP fixtures before explicit enable: "
                f"{startup_fixture_records!r}"
            )

        # Startup deliberately registers persisted MCP configurations without
        # touching credentials or starting transports. Exercise the explicit
        # activation path before verifying catalogs and shutdown/reaping.
        activated_servers: set[str] = set()
        for server in server_names:
            write_terminal_line(terminal, f"/mcp enable {server}")
            activated_servers.add(server)
            activation_deadline = time.monotonic() + 10
            activated = False
            while time.monotonic() < activation_deadline:
                check_sample_cancellation()
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
                    parse_status(
                        bytes(terminal_output),
                        server_names,
                        expected_ready=activated_servers,
                    )
                    activated = True
                if activated:
                    break
                if observe_exit(process.pid) is not None:
                    break
            if not activated:
                raise RuntimeError(
                    f"sample {sample} did not activate MCP {server}: "
                    f"{terminal_output[-5000:]!r}"
                )
        descendants = descendant_processes(process.pid)
        fixture_records = fixture_processes(descendants, fixture)
        child_groups = {record[2] for record in descendants} - {process.pid}

        exit_deadline = time.monotonic() + 5
        expected_prompts = 2 + len(server_names)
        while (
            terminal_output.count(b"rw> ") < expected_prompts
            and time.monotonic() < exit_deadline
        ):
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
        if terminal_output.count(b"rw> ") < expected_prompts:
            raise RuntimeError(
                f"sample {sample} line client did not return after MCP activation: "
                f"{terminal_output[-3000:]!r}"
            )
        # The bounded line client accepts Ctrl-D at an empty prompt as EOF,
        # then awaits its normal MCP shutdown path.
        terminal.write(b"\x04", deadline=time.monotonic() + 10)
        shutdown_deadline = time.monotonic() + 10
        while time.monotonic() < shutdown_deadline:
            check_sample_cancellation()
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
            if observe_exit(process.pid) is not None:
                break
        if observe_exit(process.pid) is None:
            raise TimeoutError("M8 normal EOF shutdown exceeded its 10s deadline")
    finally:
        terminal.close()
    if process.returncode != 0:
        raise RuntimeError(
            f"sample {sample} failed rc={process.returncode}: "
            f"terminal={bytes(terminal_output)!r} stderr={bytes(captured_stderr)!r}"
        )
    fixture_pids = {pid for pid, _ in fixture_records}
    fixture_groups = {group for _, group in fixture_records}
    if len(fixture_pids) != 3 or len(fixture_groups) != 3:
        raise RuntimeError(
            f"sample {sample} did not expose three exact approved fixture images in distinct "
            f"groups after explicit activation: {fixture_records!r}"
        )
    leaked = group_members(child_groups)
    if leaked:
        raise UnsettledScope(
            f"sample {sample} did not shutdown/reap complete MCP child groups: {leaked!r}"
        )
    # Observed group IDs are absence checks only, never signal authority.
    for group in child_groups:
        require_group_disappearance(group, timeout=0)
    shutil.rmtree(sample_root)
    return prompt_ready_ms


def percentile(values: list[float], quantile: float) -> float:
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * quantile) - 1)]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rw", type=pathlib.Path)
    parser.add_argument("--candidate", type=pathlib.Path)
    parser.add_argument("--fixture-receipt", type=pathlib.Path)
    parser.add_argument("--fixture", type=pathlib.Path)
    parser.add_argument("--samples", type=int, default=100)
    parser.add_argument("--functional-only", action="store_true")
    parser.add_argument("--metrics-json", type=pathlib.Path)
    return parser.parse_args()


def run(args, source_rw, source_fixture, measurements: list[float], expected_hashes: tuple[str, str]) -> int:
    if args.samples < 100 and not args.functional_only:
        raise RuntimeError("M8 p99 release gate requires at least 100 samples")
    if not 1 <= args.samples <= 5000:
        raise RuntimeError("M8 gate requires between one and 5000 samples")
    if args.metrics_json is not None and args.functional_only:
        raise RuntimeError("metric output requires the complete M8 performance gate")
    if not source_rw.is_file() or not source_fixture.is_file():
        raise RuntimeError("release rw and rw-mcp-fixture binaries must exist")
    with delegated_success_scope(), retained_scratch(
        "rw8-", parent=pathlib.Path(tempfile.gettempdir()),
        evidence=lambda path: print(json.dumps({"retained_scratch": str(path)}), file=sys.stderr),
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
        copied = tuple(m8_inputs.native_candidate.hash_file(path) for path in (rw, fixture))
        if copied != expected_hashes:
            raise ValueError("M8 private executable copy differs from its admitted source bytes")
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
        grant_exact_project_trust(rw, workspace, env, agents / "mcp.toml")
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
        for sample in range(args.samples):
            measurements.append(one_sample(
                rw, workspace, home, samples_root / f"sample-{sample}",
                provider_script, fixture, server_names, sample,
            ))
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


def main() -> int:
    args = parse_args()
    if args.functional_only:
        if args.rw is None or args.fixture is None or args.candidate is not None or args.fixture_receipt is not None:
            raise ValueError("functional M8 requires explicit nonqualifying engine and fixture artifacts")
        sources = (args.rw.resolve(strict=True), args.fixture.resolve(strict=True))
        return run(args, *sources, [], tuple(m8_inputs.native_candidate.hash_file(path) for path in sources))
    if args.candidate is None or args.fixture_receipt is None or args.rw is not None or args.fixture is not None:
        raise ValueError("M8 qualification requires candidate and prepared fixture receipt")
    before = m8_inputs.verify(args.candidate, args.fixture_receipt, REPO)
    engine = args.candidate / before["candidate_receipt"]["components"]["engine"]["path"]
    fixture = args.fixture_receipt.parent / m8_inputs.FIXTURE
    # Identity and physical closure both precede the gate's success acknowledgement.
    samples: list[float] = []
    record = {"schema_version": 1, "status": "running", "inputs_before": before,
              "sample_count": args.samples, "prompt_ready_ms": samples}
    with delegated_success_scope():
        try:
            hashes = (before["candidate_receipt"]["components"]["engine"]["sha256"],
                      before["prepared"]["fixture"]["sha256"])
            status = run(args, engine, fixture, samples, hashes)
            record["status"] = "pass"
            return status
        except BaseException as error:
            record.update(status="UNSETTLED" if isinstance(error, UnsettledScope) else "fail",
                          error=str(error)[-4096:])
            raise
        finally:
            try:
                after = m8_inputs.verify(args.candidate, args.fixture_receipt, REPO)
                record["inputs_after"] = after
                if before != after:
                    raise ValueError("M8 inputs changed during acceptance")
            except BaseException as error:
                record.update(status="fail", verification_error=str(error)[-4096:])
                raise
            finally:
                if args.metrics_json is not None:
                    destination = args.metrics_json.with_suffix(".evidence.json")
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    staging = destination.with_suffix(".tmp")
                    staging.write_text(json.dumps(record, sort_keys=True) + "\n")
                    staging.replace(destination)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:  # noqa: BLE001 - gate prints one actionable failure
        print(f"M8 release gate failed: {error}", file=os.sys.stderr)
        raise SystemExit(1) from error
