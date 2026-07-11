#!/usr/bin/env python3
"""Drive the production supervised TUI/engine pair and enforce its RSS budget."""

from __future__ import annotations

import argparse
import json
import math
import os
import pty
import select
import signal
import stat
import subprocess
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path


DEFAULT_SECONDS = 8 * 60 * 60
DEFAULT_RSS_LIMIT_MIB = 500
DEFAULT_TURN_SECONDS = 2.0
KITTY_SUBMIT = b"\x1b[13;3u"


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
        if index % compact_every == 0:
            kind = "compact"
            prompt = "/compact retain soak workload intent"
            scripts.append(text_events(marker, index))
        elif index % tool_every == 0:
            kind = "tool"
            prompt = f"Read soak.txt and report its stable contents.{prompt_filler}"
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
            prompt = f"Acknowledge deterministic workload turn {index:06d}.{prompt_filler}"
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
                encoded_marker = marker.encode() if marker is not None else None
                marker_index = (
                    combined.find(encoded_marker) if encoded_marker is not None else -1
                )
                if marker is not None and marker_index >= 0:
                    self.seen_markers.add(marker)
                    self.marker_locations[marker] = (
                        path,
                        max(0, offset - len(tail) + marker_index),
                    )
                    found = True
            except FileNotFoundError:
                continue
        return found

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
    tui: Path,
    duration: float,
    sample_seconds: float,
    rss_limit: int,
    turn_seconds: float = DEFAULT_TURN_SECONDS,
    compact_every: int = 8,
    tool_every: int = 5,
    restart_after_turns: int = 3,
    script_delay_ms: int = 10,
) -> dict[str, object]:
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
            "ROTTWEILER_TUI_BIN": str(tui),
            "ROTTWEILER_DRIVER_READY_MARKER": "SOAK_DRIVER_READY",
            "TERM": "xterm-256color",
        }
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
                elif waiting is not None and now - waiting_since > 60:
                    raise RuntimeError(
                        f"workload step {waiting.marker} did not persist within 60 seconds"
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
                    tui_pid = find_descendant(rows, process.pid, tui)
                    if engine_pid is not None and tui_pid is not None:
                        os.kill(tui_pid, signal.SIGKILL)
                        restart_old_tui = tui_pid
                        restart_engine = engine_pid
                        restart_deadline = now + 15

                if restart_old_tui is not None:
                    rows = process_table()
                    current_engine = find_descendant(rows, process.pid, rw, " serve ")
                    current_tui = find_descendant(rows, process.pid, tui)
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
                    # OpenTUI's multiline composer reserves plain Return for a
                    # newline. Its production submit binding is Meta+Return in
                    # Kitty keyboard-protocol form (the same sequence as M4).
                    os.write(master, waiting.prompt.encode("utf-8") + KITTY_SUBMIT)
                    submitted += 1
                    waiting_since = now

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


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rw", required=True, type=Path)
    parser.add_argument("--tui", required=True, type=Path)
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
    result = run_soak(
        validate_executable(args.rw, "rw"),
        validate_executable(args.tui, "TUI"),
        args.duration_seconds,
        args.sample_seconds,
        args.rss_limit_mib * 1024 * 1024,
        args.turn_seconds,
        args.compact_every,
        args.tool_every,
        args.restart_after_turns,
        args.script_delay_ms,
    )
    args.output.write_text(json.dumps(result, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
