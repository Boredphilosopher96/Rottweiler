#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
cd "$repo"

export ROTTWEILER_CREDENTIAL_BACKEND=file
if [ -n "${ROTTWEILER_PERF_PREBUILT_RW:-}" ]; then
  case $ROTTWEILER_PERF_PREBUILT_RW in
    /*) ;;
    *)
      echo "ROTTWEILER_PERF_PREBUILT_RW must be an absolute path" >&2
      exit 2
      ;;
  esac
  built_binary=$(python3 - "$ROTTWEILER_PERF_PREBUILT_RW" "${RUNNER_TEMP:-}" "${GITHUB_ACTIONS:-}" <<'PY'
import os
import pathlib
import stat
import sys

path = pathlib.Path(sys.argv[1])
metadata = path.lstat()
mode = stat.S_IMODE(metadata.st_mode)
if (
    not stat.S_ISREG(metadata.st_mode)
    or metadata.st_nlink != 1
    or not mode & stat.S_IXUSR
    or mode & 0o077
):
    raise SystemExit(
        "ROTTWEILER_PERF_PREBUILT_RW must be an owner-private, single-link regular executable"
    )
resolved = path.resolve(strict=True)
if sys.argv[3] == "true":
    if not sys.argv[2]:
        raise SystemExit("RUNNER_TEMP is required for a CI prebuilt performance binary")
    runner_temp = pathlib.Path(sys.argv[2]).resolve(strict=True)
    if os.path.commonpath((resolved, runner_temp)) != str(runner_temp):
        raise SystemExit("CI prebuilt performance binary must remain under RUNNER_TEMP")
print(resolved)
PY
  )
  using_prebuilt=1
else
  scripts/cargo-release.sh build --locked --release -p rw-cli
  release_dir=$(scripts/cargo-release.sh artifact-dir)
  built_binary=$release_dir/rw
  using_prebuilt=0
fi

measurement_parent=${RUNNER_TEMP:-${TMPDIR:-/tmp}}
temporary_root=$(mktemp -d "$measurement_parent/rottweiler-perf.XXXXXX")
root=$temporary_root.noindex
mv "$temporary_root" "$root"
trap 'rm -rf "$root"' EXIT HUP INT TERM

python3 - "$repo" "$root" "${ROTTWEILER_PERF_OUTPUT:-}" "$built_binary" "$using_prebuilt" <<'PY'
import json
import math
import os
import pathlib
import platform
import shutil
import stat
import statistics
import subprocess
import sys
import time

repo = pathlib.Path(sys.argv[1])
root = pathlib.Path(sys.argv[2])
output = pathlib.Path(sys.argv[3]) if sys.argv[3] else None
built_binary = pathlib.Path(sys.argv[4])
using_prebuilt = sys.argv[5] == "1"
binary = root / "rw"
open_flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
source_fd = os.open(built_binary, open_flags)
with os.fdopen(source_fd, "rb") as source:
    source_metadata = os.fstat(source.fileno())
    source_mode = stat.S_IMODE(source_metadata.st_mode)
    if using_prebuilt and (
        not stat.S_ISREG(source_metadata.st_mode)
        or source_metadata.st_uid != os.geteuid()
        or source_metadata.st_nlink != 1
        or not source_mode & stat.S_IXUSR
        or source_mode & 0o077
    ):
        raise SystemExit(
            "ROTTWEILER_PERF_PREBUILT_RW changed after validation or is not owner-private"
        )
    with binary.open("xb") as destination:
        shutil.copyfileobj(source, destination)
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

smoke = os.environ.get("ROTTWEILER_PERF_SMOKE") == "1"
sample_count = int(os.environ.get("ROTTWEILER_PERF_SAMPLES", "100" if smoke else "500"))
minimum_samples = 100
if sample_count < minimum_samples or sample_count > 5000:
    raise SystemExit(
        f"ROTTWEILER_PERF_SAMPLES must be between {minimum_samples} and 5000"
    )

# A fat-LTO link leaves hosted Apple runners hot while macOS may still inspect
# the newly installed executable. Give Apple hosts one fixed cooling/inspection
# interval, then use five fixed fresh-process warmups. Smoke mode reduces only
# the measured sample count; it keeps identical host conditioning so its p99
# enforces the same absolute contract instead of measuring cold-runner noise.
# Measured results are never retried or trimmed, and even smoke mode retains
# the 100-sample floor required for a meaningful empirical p99.
time.sleep(60 if sys.platform == "darwin" else 1)
for index in range(-5, 0):
    one(index)
samples = [one(index) for index in range(sample_count)]
starts = sorted(sample[0] for sample in samples)
turns = sorted(sample[1] for sample in samples)
p95_index = math.ceil(len(samples) * 0.95) - 1
p99_index = math.ceil(len(samples) * 0.99) - 1
start_p99 = starts[p99_index]
turn_p95 = turns[p95_index]
turn_p99 = turns[p99_index]
binary_bytes = binary.stat().st_size
if output is not None:
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = output.with_name(f".{output.name}.tmp")
    temporary.write_text(json.dumps({
        "schema_version": 1,
        "metrics": {
            "engine_binary_bytes": binary_bytes,
            "headless_print_p99_us": math.ceil(start_p99 * 1000),
            "turn_overhead_p99_us": math.ceil(turn_p99 * 1000),
        },
    }, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(output)

    def bounded(value, limit=128):
        return value[:limit] if isinstance(value, str) else None

    evidence = output.with_name(f"{output.stem}.evidence{output.suffix}")
    evidence_temporary = evidence.with_name(f".{evidence.name}.tmp")
    evidence_temporary.write_text(json.dumps({
        "schema_version": 1,
        "sample_count": sample_count,
        "samples": [
            {
                "index": index,
                "headless_print_us": math.ceil(start_ms * 1000),
                "turn_overhead_us": math.ceil(turn_ms * 1000),
            }
            for index, (start_ms, turn_ms) in enumerate(samples)
        ],
        "runner": {
            "github_actions": os.environ.get("GITHUB_ACTIONS") == "true",
            "image_os": bounded(os.environ.get("ImageOS")),
            "image_version": bounded(os.environ.get("ImageVersion")),
            "machine": bounded(platform.machine()),
            "os": bounded(platform.system()),
            "os_release": bounded(platform.release()),
            "python_version": bounded(platform.python_version()),
            "runner_arch": bounded(os.environ.get("RUNNER_ARCH")),
            "runner_environment": bounded(os.environ.get("RUNNER_ENVIRONMENT")),
            "runner_os": bounded(os.environ.get("RUNNER_OS")),
        },
    }, sort_keys=True) + "\n", encoding="utf-8")
    evidence_temporary.replace(evidence)
print(
    f"samples={sample_count}; "
    f"headless_print_ms p50={statistics.median(starts):.3f} "
    f"p95={starts[p95_index]:.3f} p99={start_p99:.3f} max={starts[-1]:.3f}; "
    f"zero_latency_turn_ms p50={statistics.median(turns):.3f} "
    f"p95={turns[p95_index]:.3f} p99={turn_p99:.3f} max={turns[-1]:.3f}"
)
if start_p99 >= 80:
    raise SystemExit(f"headless print-mode p99 {start_p99:.3f}ms exceeds 80ms")
if smoke and turn_p95 >= 20:
    raise SystemExit(f"zero-latency full-turn smoke p95 {turn_p95:.3f}ms exceeds 20ms")
if smoke and turn_p99 >= 40:
    raise SystemExit(f"zero-latency full-turn smoke p99 {turn_p99:.3f}ms exceeds 40ms")
if not smoke and turn_p99 >= 20:
    raise SystemExit(f"zero-latency full-turn p99 {turn_p99:.3f}ms exceeds 20ms")
if binary_bytes >= 25_000_000:
    raise SystemExit(f"release binary size {binary_bytes} exceeds 25MB")
PY
