#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
cd "$repo"

export ROTTWEILER_CREDENTIAL_BACKEND=file
cargo build --locked --release -p rw-cli

root=$(mktemp -d "${TMPDIR:-/tmp}/rottweiler-perf.XXXXXX")
trap 'rm -rf "$root"' EXIT HUP INT TERM

python3 - "$repo" "$root" <<'PY'
import math
import os
import pathlib
import shutil
import statistics
import subprocess
import sys
import time

repo = pathlib.Path(sys.argv[1])
root = pathlib.Path(sys.argv[2])
built_binary = repo / "target/release/rw"
binary = root / "rw"
shutil.copyfile(built_binary, binary)
binary.chmod(0o700)
script = repo / "crates/rw-cli/tests/fixtures/perf-script.json"

def one(index):
    home = root / f"home-{index}"
    home.mkdir()
    home.chmod(0o700)
    env = {
        "HOME": str(home),
        "ROTTWEILER_HOME": str(home),
        "ROTTWEILER_CREDENTIAL_BACKEND": "file",
        "PATH": os.environ["PATH"],
    }
    started = time.perf_counter_ns()
    run = subprocess.run(
        [
            str(binary),
            "-p", "perf",
            "--permission-mode", "yolo",
            "--in-memory-replay-script", str(script),
            "--output-format", "text",
            "--perf-markers",
        ],
        cwd=repo,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
    if run.returncode != 0 or run.stdout.strip() != b"ready":
        raise SystemExit(
            f"release replay failed rc={run.returncode}: {run.stderr.decode(errors='replace')}"
        )
    markers = {}
    for line in run.stderr.decode().splitlines():
        if line.startswith("rw_perf_"):
            key, value = line.split("=", 1)
            markers[key] = int(value) / 1000
    try:
        turn_ms = markers["rw_perf_zero_latency_turn_us"]
    except KeyError as error:
        raise SystemExit(f"missing performance marker: {error}") from error
    return elapsed_ms, turn_ms

# Warm executable pages and the deterministic fixture before collecting p99.
one(-1)
sample_count = int(os.environ.get("ROTTWEILER_PERF_SAMPLES", "500"))
if sample_count < 100:
    raise SystemExit("ROTTWEILER_PERF_SAMPLES must be at least 100")
samples = [one(index) for index in range(sample_count)]
starts = sorted(sample[0] for sample in samples)
turns = sorted(sample[1] for sample in samples)
p95_index = math.ceil(len(samples) * 0.95) - 1
p99_index = math.ceil(len(samples) * 0.99) - 1
start_p99 = starts[p99_index]
turn_p99 = turns[p99_index]
print(
    f"samples={sample_count}; "
    f"headless_print_ms p50={statistics.median(starts):.3f} "
    f"p95={starts[p95_index]:.3f} p99={start_p99:.3f} max={starts[-1]:.3f}; "
    f"zero_latency_turn_ms p50={statistics.median(turns):.3f} "
    f"p95={turns[p95_index]:.3f} p99={turn_p99:.3f} max={turns[-1]:.3f}"
)
if start_p99 >= 80:
    raise SystemExit(f"headless print-mode p99 {start_p99:.3f}ms exceeds 80ms")
if turn_p99 >= 20:
    raise SystemExit(f"zero-latency full-turn p99 {turn_p99:.3f}ms exceeds 20ms")
if built_binary.stat().st_size >= 25_000_000:
    raise SystemExit(f"release binary size {built_binary.stat().st_size} exceeds 25MB")
PY
