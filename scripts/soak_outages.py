"""Bounded receive-clock evidence for restoration and visible input after recycling."""
from __future__ import annotations

START = b"SOAK_TUI_PROCESS_START"
READY = b"SOAK_DRIVER_READY"
INPUT = b"SOAK_TUI_INPUT_ACK"
MARKERS = {START: "start", READY: "ready", INPUT: "input"}
MAX_GENERATIONS = 4096


class Outages:
    def __init__(self, started: float):
        self.started = started
        self.tail = b""
        self.overlong_line = False
        self.generations: list[dict] = []
        self.forced_at: tuple[float, float] | None = None

    def force(self, before: float, after: float) -> None:
        if self.forced_at is not None:
            raise ValueError("overlapping forced TUI outage")
        self.forced_at = (before, after)

    def confirm_ready(self, at: float, pid: int) -> None:
        if self.generations and self.generations[-1].get("pid") == pid:
            self.generations[-1]["last_confirmed_ready_seconds"] = at - self.started

    def feed(self, chunk: bytes, at: float, ready_pid) -> int:
        # Only newline-delimited fixed product markers are retained. Arbitrary
        # terminal frames cannot grow the partial marker buffer.
        data = self.tail + chunk
        lines = data.split(b"\n")
        complete = lines[:-1]
        if self.overlong_line and complete:
            complete[0] = b""
            self.overlong_line = False
        if len(lines[-1]) > max(map(len, MARKERS)) + 1:
            self.overlong_line = True
            self.tail = b""
        else:
            self.tail = lines[-1]
        ready_count = 0
        for line in complete:
            kind = MARKERS.get(line.rstrip(b"\r"))
            if kind is None:
                continue
            elapsed = at - self.started
            if kind == "start":
                if len(self.generations) == MAX_GENERATIONS:
                    raise ValueError("soak exceeded 4096 TUI generation records")
                previous = self.generations[-1] if self.generations else None
                record = {"generation": len(self.generations), "process_start_seconds": elapsed,
                          "cause": "initial" if previous is None else "observed_restart"}
                if previous is not None:
                    if "input_ack_seconds" not in previous:
                        raise ValueError("TUI replaced before its visible input acknowledgement")
                    record["previous_ready_observed_seconds"] = previous["last_confirmed_ready_seconds"]
                if self.forced_at is not None:
                    record.update(cause="forced_fault", fault_seconds=self.forced_at[0] - self.started,
                                  fault_signal_completed_seconds=self.forced_at[1] - self.started)
                    self.forced_at = None
                self.generations.append(record)
            elif not self.generations:
                raise ValueError("TUI readiness/input marker preceded its process-start marker")
            elif kind == "ready":
                record = self.generations[-1]
                if "driver_ready_seconds" in record:
                    raise ValueError("duplicate TUI driver readiness in one generation")
                pid = ready_pid()
                if type(pid) is not int or pid <= 0:
                    raise ValueError("TUI readiness lacks an observed current process identity")
                record.update(pid=pid, driver_ready_seconds=elapsed, last_confirmed_ready_seconds=elapsed,
                              restoration_ms=1000 * (elapsed - record["process_start_seconds"]))
                ready_count += 1
            else:
                record = self.generations[-1]
                if "driver_ready_seconds" not in record or "input_ack_seconds" in record:
                    raise ValueError("TUI input acknowledgement lacks unique driver readiness")
                record.update(input_ack_seconds=elapsed,
                              ready_to_visible_input_ms=1000 * (elapsed - record["driver_ready_seconds"]))
                if "fault_seconds" in record:
                    record["forced_input_blackout_ms"] = 1000 * (elapsed - record["fault_seconds"])
                    record["fault_signal_interval_ms"] = 1000 * (record["fault_signal_completed_seconds"] - record["fault_seconds"])
                elif record["generation"] > 0:
                    record["natural_blackout_observed_bounds_ms"] = [
                        1000 * (elapsed - record["process_start_seconds"]),
                        1000 * (elapsed - record["previous_ready_observed_seconds"]),
                    ]
        return ready_count

    def snapshot(self, *, complete: bool = True) -> dict:
        rows = self.generations if complete else self.generations[-16:]
        return {"clock": "observer_monotonic_receive", "generation_count": len(self.generations),
                "records_complete": complete, "records": [dict(row) for row in rows],
                "forced_fault_pending": self.forced_at is not None,
                "natural_interval": "bounds between last observed ready process and new-process marker; not an exact retirement timestamp"}

    def require_complete(self) -> None:
        if self.forced_at is not None or not self.generations:
            raise ValueError("soak lacks completed TUI generation evidence")
        if any("input_ack_seconds" not in row for row in self.generations):
            raise ValueError("soak ended without visible input acknowledgement after TUI startup/recycle")
        if not any("forced_input_blackout_ms" in row for row in self.generations):
            raise ValueError("soak lacks actual forced-restart input blackout measurement")
