#!/usr/bin/env python3
"""Drive the production supervised TUI/engine pair and enforce its RSS budget."""

from __future__ import annotations

import argparse
import json
import math
import os
import pty
import re
import select
import signal
import stat
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path


DEFAULT_SECONDS = 8 * 60 * 60
DEFAULT_RSS_LIMIT_MIB = 500
DEFAULT_TURN_SECONDS = 2.0
TERMINAL_SUBMIT = b"\r"
SOAK_TOKEN = re.compile(rb"SOAK_(?:INPUT|STEP)_[0-9]{6}(?:_DONE)?")
EVENT_TYPE = re.compile(rb'"type"\s*:\s*"([a-z0-9_]+)"')


@dataclass(frozen=True)
class ProcessRow:
    parent: int
    rss: int
    command: str


@dataclass(frozen=True)
class WorkloadStep:
    prompt: str
    marker: str
    kind: str


def parse_process_table(output: str) -> dict[int, ProcessRow]:
    rows: dict[int, ProcessRow] = {}
    for line in output.splitlines():
        fields = line.strip().split(maxsplit=3)
        if len(fields) >= 3 and all(field.isdigit() for field in fields[:3]):
            rows[int(fields[0])] = ProcessRow(
                parent=int(fields[1]),
                rss=int(fields[2]) * 1024,
                command=fields[3] if len(fields) == 4 else "",
            )
    return rows


def process_table() -> dict[int, ProcessRow]:
    output = subprocess.run(
        ["ps", "-axo", "pid=,ppid=,rss=,command="],
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    ).stdout
    return parse_process_table(output)


def descendants(rows: dict[int, ProcessRow], root_pid: int) -> set[int]:
    selected = {root_pid}
    changed = True
    while changed:
        changed = False
        for pid, row in rows.items():
            if row.parent in selected and pid not in selected:
                selected.add(pid)
                changed = True
    return selected & rows.keys()


def process_rss(root_pid: int) -> tuple[int, int]:
    rows = process_table()
    selected = descendants(rows, root_pid)
    return sum(rows[pid].rss for pid in selected), len(selected)


def find_descendant(
    rows: dict[int, ProcessRow], root_pid: int, executable: Path, required: str = ""
) -> int | None:
    executable_text = str(executable)
    for pid in sorted(descendants(rows, root_pid)):
        if pid == root_pid:
            continue
        command = rows[pid].command
        if executable_text in command and (not required or required in command):
            return pid
    return None


def validate_executable(path: Path, label: str) -> Path:
    path = path.resolve(strict=True)
    metadata = path.stat()
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
        raise ValueError(f"{label} must be a single-link regular file")
    if not os.access(path, os.X_OK):
        raise ValueError(f"{label} is not executable")
    return path


def terminate_group(process: subprocess.Popen[bytes]) -> None:
    terminate_tree(process, {})


def pid_exists(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def terminate_tree(
    process: subprocess.Popen[bytes], owned_processes: dict[int, str]
) -> None:
    """Gracefully stop the supervisor, then kill every retained owned group."""
    if process.poll() is None:
        try:
            rows = process_table()
            selected = descendants(rows, process.pid)
            owned_processes.clear()
            owned_processes.update({pid: rows[pid].command for pid in selected})
        except (OSError, subprocess.SubprocessError):
            pass
        try:
            # Signal only the supervisor first so its managed-child cleanup can
            # terminate and wait for the TUI and independently grouped engine.
            os.kill(process.pid, signal.SIGTERM)
            process.wait(timeout=5)
        except ProcessLookupError:
            pass
        except subprocess.TimeoutExpired:
            pass

    try:
        current_rows = process_table()
    except (OSError, subprocess.SubprocessError):
        current_rows = {}
    # Retaining historical PIDs across an eight-hour run risks PID reuse. The
    # latest snapshot replaces older ones, and fallback signals only a PID
    # whose current command still exactly matches the owned process.
    live = {
        pid
        for pid, command in owned_processes.items()
        if pid in current_rows and current_rows[pid].command == command
    }
    groups: set[int] = set()
    for pid in live:
        try:
            group = os.getpgid(pid)
            if group != os.getpgrp():
                groups.add(group)
        except ProcessLookupError:
            pass
    for group in groups:
        try:
            os.killpg(group, signal.SIGTERM)
        except (ProcessLookupError, PermissionError):
            pass
    deadline = time.monotonic() + 2
    while time.monotonic() < deadline and any(pid_exists(pid) for pid in live):
        time.sleep(0.02)
    for group in groups:
        try:
            os.killpg(group, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass
    for pid in live:
        if pid_exists(pid):
            try:
                os.kill(pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
    if process.poll() is None:
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=2)


def text_events(marker: str, index: int) -> list[dict[str, object]]:
    filler = f" streamed workload {index:06d} " + ("0123456789abcdef" * 16)
    return [
        {"type": "text_delta", "text": marker},
        {"type": "text_delta", "text": filler},
        {"type": "text_delta", "text": filler},
        {"type": "text_delta", "text": filler},
        {"type": "finished", "reason": "stop"},
    ]


def build_workload(
    count: int, compact_every: int, tool_every: int
) -> tuple[list[WorkloadStep], list[list[dict[str, object]]]]:
    if count <= 0 or compact_every <= 0 or tool_every <= 0:
        raise ValueError("workload counts must be positive")
    steps: list[WorkloadStep] = []
    scripts: list[list[dict[str, object]]] = []
    prompt_filler = " retain production transcript state " + ("abcdefghij" * 32)
    for index in range(1, count + 1):
        marker = f"SOAK_STEP_{index:06d}_DONE"
        input_marker = f"SOAK_INPUT_{index:06d}"
        if index % compact_every == 0:
            kind = "compact"
            prompt = f"/compact retain soak workload intent {input_marker}"
            scripts.append(text_events(marker, index))
        elif index % tool_every == 0:
            kind = "tool"
            prompt = (
                f"Read soak.txt and report its stable contents. {input_marker}"
                f"{prompt_filler}"
            )
            call_id = f"soak-read-{index:06d}"
            scripts.append(
                [
                    {"type": "tool_call_start", "id": call_id, "name": "read"},
                    {
                        "type": "tool_call_end",
                        "id": call_id,
                        "arguments": {"path": "soak.txt"},
                    },
                    {"type": "finished", "reason": "tool_calls"},
                ]
            )
            scripts.append(text_events(marker, index))
        else:
            kind = "turn"
            prompt = (
                f"Acknowledge deterministic workload turn {index:06d}. "
                f"{input_marker}{prompt_filler}"
            )
            scripts.append(text_events(marker, index))
        steps.append(WorkloadStep(prompt=prompt, marker=marker, kind=kind))
    return steps, scripts


class EventLogProbe:
    """Incrementally observes durable logs without repeatedly rereading them."""

    def __init__(self, sessions_root: Path) -> None:
        self.sessions_root = sessions_root
        self.offsets: dict[Path, int] = {}
        self.tails: dict[Path, bytes] = {}
        self.seen_markers: set[str] = set()
        self.marker_locations: dict[str, tuple[Path, int]] = {}
        self.event_counts: dict[str, int] = {}
        self.bytes_observed = 0

    def poll(self, marker: str | None = None) -> bool:
        found = marker in self.seen_markers if marker is not None else False
        for path in sorted(self.sessions_root.glob("*/events.jsonl")):
            try:
                size = path.stat().st_size
                offset = self.offsets.get(path, 0)
                if size < offset:
                    offset = 0
                if size == offset:
                    continue
                with path.open("rb") as handle:
                    handle.seek(offset)
                    raw = handle.read()
                self.offsets[path] = offset + len(raw)
                self.bytes_observed += len(raw)
                tail = self.tails.get(path, b"")
                combined = tail + raw
                self.tails[path] = combined[-256:]
                for match in SOAK_TOKEN.finditer(combined):
                    if match.end() <= len(tail):
                        continue
                    token = match.group().decode("ascii")
                    self.seen_markers.add(token)
                    self.marker_locations[token] = (
                        path,
                        max(0, offset - len(tail) + match.start()),
                    )
                for match in EVENT_TYPE.finditer(combined):
                    if match.end() <= len(tail):
                        continue
                    event_type = match.group(1).decode("ascii")
                    self.event_counts[event_type] = (
                        self.event_counts.get(event_type, 0) + 1
                    )
                if marker is not None and marker in self.seen_markers:
                    found = True
            except FileNotFoundError:
                continue
        return found

    def saw(self, marker: str) -> bool:
        return marker in self.seen_markers

    def event_count(self, event_type: str) -> int:
        return self.event_counts.get(event_type, 0)

    def marker_persisted(self, marker: str) -> bool:
        """Re-read only the exact recorded marker range from the durable log."""
        location = self.marker_locations.get(marker)
        if location is None:
            return False
        path, offset = location
        encoded = marker.encode()
        try:
            if path.stat().st_size < offset + len(encoded):
                return False
            with path.open("rb") as handle:
                handle.seek(offset)
                return handle.read(len(encoded)) == encoded
        except FileNotFoundError:
            return False

    def durable_bytes(self) -> int:
        total = 0
        for path in self.sessions_root.glob("*/events.jsonl"):
            try:
                total += path.stat().st_size
            except FileNotFoundError:
                pass
        return total


def write_replay_script(
    path: Path, duration: float, turn_seconds: float, compact_every: int, tool_every: int
) -> list[WorkloadStep]:
    count = max(math.ceil(duration / turn_seconds) + 32, compact_every + 1, tool_every + 1)
    steps, scripts = build_workload(count, compact_every, tool_every)
    path.write_text(json.dumps(scripts, separators=(",", ":")), encoding="utf-8")
    return steps


def run_soak(
    rw: Path,
    tui: Path | None,
    duration: float,
    sample_seconds: float,
    rss_limit: int,
    turn_seconds: float = DEFAULT_TURN_SECONDS,
    compact_every: int = 8,
    tool_every: int = 5,
    restart_after_turns: int = 3,
    script_delay_ms: int = 10,
) -> dict[str, object]:
    tui_executable = validate_executable(
        tui if tui is not None else rw.with_name("rottweiler-tui"),
        "TUI",
    )
    if duration <= 0 or sample_seconds <= 0 or turn_seconds <= 0:
        raise ValueError("duration and sample intervals must be positive")
    if compact_every <= 0 or tool_every <= 0 or restart_after_turns <= 0:
        raise ValueError("workload frequencies must be positive")
    if script_delay_ms < 0:
        raise ValueError("script delay must not be negative")

    master, slave = pty.openpty()
    os.set_blocking(master, False)
    started = time.monotonic()
    maximum_rss = 0
    maximum_processes = 0
    samples = 0
    submitted = 0
    completed = 0
    streamed_turns = 0
    tool_turns = 0
    compactions = 0
    tui_restarts = 0

    # Unix-domain sockets have a small platform path limit (104 bytes on
    # macOS). Keep the private harness root short before the supervisor adds
    # its randomized runtime directory.
    with tempfile.TemporaryDirectory(prefix="rws-", dir="/tmp") as temporary:
        root = Path(temporary)
        home = root / "h"
        workspace = root / "w"
        state = root / "s"
        home.mkdir(mode=0o700)
        workspace.mkdir(mode=0o700)
        workspace.joinpath("soak.txt").write_text("stable soak fixture\n", encoding="utf-8")
        replay_script = root / "provider-script.json"
        steps = write_replay_script(
            replay_script, duration, turn_seconds, compact_every, tool_every
        )
        environment = {
            "HOME": str(home),
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "ROTTWEILER_CREDENTIAL_BACKEND": "file",
            "ROTTWEILER_HOME": str(state),
            "ROTTWEILER_DRIVER_READY_MARKER": "SOAK_DRIVER_READY",
            "TERM": "xterm-256color",
        }
        if tui is not None:
            environment["ROTTWEILER_TUI_BIN"] = str(tui_executable)
        process = subprocess.Popen(
            [
                str(rw),
                "--permission-mode",
                "auto-safe",
                "--in-memory-replay-script",
                str(replay_script),
                "--record-script-delay-ms",
                str(script_delay_ms),
            ],
            cwd=workspace,
            env=environment,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            start_new_session=True,
        )
        os.close(slave)
        owned_processes: dict[int, str] = {}
        # The isolated workspace has no executable project configuration. Deny
        # the one-time trust prompt explicitly so the production supervisor starts.
        os.write(master, b"n\r")
        probe = EventLogProbe(state / "sessions")
        waiting: WorkloadStep | None = None
        waiting_since = 0.0
        waiting_accepted = False
        waiting_submissions = 0
        waiting_compaction_started = 0
        waiting_compaction_finished = 0
        acceptance_deadline = 0.0
        next_submit = started + 1.0
        next_sample = started
        restart_old_tui: int | None = None
        restart_engine: int | None = None
        restart_deadline = 0.0
        last_completed_marker: str | None = None
        terminal_tail = bytearray()
        engine_diagnostic = "not observed"
        driver_ready_count = 0

        try:
            while time.monotonic() - started < duration:
                if process.poll() is not None:
                    raise RuntimeError(
                        "supervised Rottweiler exited early with "
                        f"{process.returncode}: "
                        f"submitted={submitted} completed={completed} "
                        f"waiting={waiting}; terminal tail: "
                        f"engine={engine_diagnostic}; "
                        f"{terminal_tail.decode('utf-8', errors='replace')[-4000:]}"
                    )
                now = time.monotonic()
                readable, _, _ = select.select([master], [], [], 0.02)
                if readable:
                    try:
                        while chunk := os.read(master, 64 * 1024):
                            driver_ready_count += chunk.count(b"SOAK_DRIVER_READY")
                            terminal_tail.extend(chunk)
                            del terminal_tail[:-16_384]
                            diagnostic_source = terminal_tail.decode(
                                "utf-8", errors="replace"
                            )
                            diagnostic_index = diagnostic_source.rfind(
                                "deterministic replay engine exited"
                            )
                            if diagnostic_index >= 0:
                                engine_diagnostic = diagnostic_source[
                                    diagnostic_index : diagnostic_index + 120
                                ].splitlines()[0]
                            if engine_diagnostic == "not observed":
                                for diagnostic_marker in ("panicked at", "Error:"):
                                    diagnostic_index = diagnostic_source.find(
                                        diagnostic_marker
                                    )
                                    if diagnostic_index >= 0:
                                        engine_diagnostic = diagnostic_source[
                                            diagnostic_index : diagnostic_index + 1000
                                        ]
                                        break
                    except BlockingIOError:
                        pass
                    except OSError:
                        if process.poll() is None:
                            raise

                if waiting is not None and probe.poll(waiting.marker):
                    completed += 1
                    last_completed_marker = waiting.marker
                    if waiting.kind == "compact":
                        compactions += 1
                    elif waiting.kind == "tool":
                        tool_turns += 1
                        streamed_turns += 1
                    else:
                        streamed_turns += 1
                    waiting = None
                    next_submit = now + turn_seconds
                elif waiting is not None:
                    input_marker = waiting.marker.replace(
                        "SOAK_STEP_", "SOAK_INPUT_"
                    ).removesuffix("_DONE")
                    if not waiting_accepted:
                        accepted = (
                            probe.event_count("compaction_started")
                            > waiting_compaction_started
                            if waiting.kind == "compact"
                            else probe.saw(input_marker)
                        )
                        if accepted:
                            waiting_accepted = True
                            waiting_since = now
                        elif now >= acceptance_deadline:
                            if waiting_submissions >= 3:
                                raise RuntimeError(
                                    f"workload step {waiting.marker} was not accepted after "
                                    f"{waiting_submissions} PTY submissions; "
                                    f"engine={engine_diagnostic}; terminal tail: "
                                    f"{terminal_tail.decode('utf-8', errors='replace')[-4000:]}"
                                )
                            os.write(
                                master, waiting.prompt.encode("utf-8") + TERMINAL_SUBMIT
                            )
                            waiting_submissions += 1
                            acceptance_deadline = now + 5
                    elif (
                        waiting.kind == "compact"
                        and probe.event_count("compaction_finished")
                        > waiting_compaction_finished
                    ):
                        raise RuntimeError(
                            f"workload step {waiting.marker} completed compaction without "
                            "persisting its replay marker"
                        )
                    elif now - waiting_since > 60:
                        raise RuntimeError(
                            f"accepted workload step {waiting.marker} did not persist within "
                            f"60 seconds; engine={engine_diagnostic}; terminal tail: "
                            f"{terminal_tail.decode('utf-8', errors='replace')[-4000:]}"
                        )
                else:
                    probe.poll()

                if (
                    tui_restarts == 0
                    and restart_old_tui is None
                    and completed >= restart_after_turns
                    and waiting is None
                ):
                    rows = process_table()
                    engine_pid = find_descendant(rows, process.pid, rw, " serve ")
                    tui_pid = find_descendant(rows, process.pid, tui_executable)
                    if engine_pid is not None and tui_pid is not None:
                        os.kill(tui_pid, signal.SIGKILL)
                        restart_old_tui = tui_pid
                        restart_engine = engine_pid
                        restart_deadline = now + 15

                if restart_old_tui is not None:
                    rows = process_table()
                    current_engine = find_descendant(rows, process.pid, rw, " serve ")
                    current_tui = find_descendant(rows, process.pid, tui_executable)
                    if (
                        current_engine == restart_engine
                        and current_tui is not None
                        and current_tui != restart_old_tui
                    ):
                        if last_completed_marker is None or not probe.marker_persisted(
                            last_completed_marker
                        ):
                            raise RuntimeError("durable transcript was lost across TUI restart")
                        tui_restarts += 1
                        restart_old_tui = None
                        next_submit = now + 0.5
                    elif now >= restart_deadline:
                        raise RuntimeError(
                            "supervisor did not reconnect a new TUI to the original engine"
                        )

                if (
                    waiting is None
                    and restart_old_tui is None
                    and driver_ready_count > tui_restarts
                    and now >= next_submit
                    and submitted < len(steps)
                ):
                    waiting = steps[submitted]
                    # The production composer submits on plain Return. The soak
                    # drives an xterm-compatible PTY, matching the M4 release
                    # gate and a physical Return key in that terminal.
                    os.write(master, waiting.prompt.encode("utf-8") + TERMINAL_SUBMIT)
                    submitted += 1
                    waiting_since = now
                    waiting_accepted = False
                    waiting_submissions = 1
                    waiting_compaction_started = probe.event_count(
                        "compaction_started"
                    )
                    waiting_compaction_finished = probe.event_count(
                        "compaction_finished"
                    )
                    acceptance_deadline = now + 5

                if now >= next_sample:
                    rows = process_table()
                    selected = descendants(rows, process.pid)
                    owned_processes = {pid: rows[pid].command for pid in selected}
                    rss = sum(rows[pid].rss for pid in selected)
                    processes = len(selected)
                    maximum_rss = max(maximum_rss, rss)
                    maximum_processes = max(maximum_processes, processes)
                    samples += 1
                    if rss > rss_limit:
                        raise RuntimeError(
                            f"combined engine/TUI RSS {rss} exceeds limit {rss_limit}"
                        )
                    next_sample = now + sample_seconds

            if maximum_processes < 3:
                raise RuntimeError(
                    "soak never observed supervisor, engine, and TUI together"
                )
            if completed < max(compact_every, tool_every):
                raise RuntimeError(
                    "soak duration was too short to complete tool and compaction workloads: "
                    f"submitted={submitted} completed={completed} "
                    f"driver_ready={driver_ready_count} waiting={waiting}"
                )
            if streamed_turns == 0 or tool_turns == 0 or compactions == 0:
                raise RuntimeError("soak did not exercise every required production path")
            if tui_restarts != 1:
                raise RuntimeError("soak did not complete the supervised TUI reconnect")
            durable_bytes = probe.durable_bytes()
            if durable_bytes <= 0 or last_completed_marker is None:
                raise RuntimeError("soak did not persist a durable transcript")
        finally:
            terminate_tree(process, owned_processes)
            os.close(master)

    elapsed = time.monotonic() - started
    return {
        "compactions_completed": compactions,
        "duration_seconds": round(elapsed, 3),
        "durable_transcript_bytes": durable_bytes,
        "max_processes": maximum_processes,
        "max_rss_bytes": maximum_rss,
        "rss_limit_bytes": rss_limit,
        "samples": samples,
        "status": "pass",
        "streamed_turns_completed": streamed_turns,
        "tool_turns_completed": tool_turns,
        "tui_restarts": tui_restarts,
        "turns_completed": completed,
        "turns_submitted": submitted,
    }


def failure_result(error: Exception) -> dict[str, object]:
    return {
        "error": str(error),
        "error_type": type(error).__name__,
        "schema_version": 1,
        "status": "fail",
    }


def write_result(path: Path, result: dict[str, object]) -> None:
    path.write_text(json.dumps(result, sort_keys=True) + "\n", encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rw", required=True, type=Path)
    parser.add_argument(
        "--tui",
        type=Path,
        help="development-only TUI override; release bundles discover the private sibling",
    )
    parser.add_argument("--duration-seconds", type=float, default=DEFAULT_SECONDS)
    parser.add_argument("--sample-seconds", type=float, default=5.0)
    parser.add_argument("--rss-limit-mib", type=int, default=DEFAULT_RSS_LIMIT_MIB)
    parser.add_argument("--turn-seconds", type=float, default=DEFAULT_TURN_SECONDS)
    parser.add_argument("--compact-every", type=int, default=8)
    parser.add_argument("--tool-every", type=int, default=5)
    parser.add_argument("--restart-after-turns", type=int, default=3)
    parser.add_argument("--script-delay-ms", type=int, default=10)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        result = run_soak(
            validate_executable(args.rw, "rw"),
            validate_executable(args.tui, "TUI") if args.tui is not None else None,
            args.duration_seconds,
            args.sample_seconds,
            args.rss_limit_mib * 1024 * 1024,
            args.turn_seconds,
            args.compact_every,
            args.tool_every,
            args.restart_after_turns,
            args.script_delay_ms,
        )
    except Exception as error:
        result = failure_result(error)
        write_result(args.output, result)
        print(json.dumps(result, sort_keys=True), file=sys.stderr)
        raise
    write_result(args.output, result)
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
