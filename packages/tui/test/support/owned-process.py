"""Run a test executable under the shared anchored process-group owner."""
from pathlib import Path
import json
import os
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "scripts"))
from perf_process import run_sample, require_sample_settlement
from owned_process_lifeline import ParentLifeline

MAX_REQUEST = 1024 * 1024
MAX_OUTPUT = 1024 * 1024


class CombinedOutput:
    def __init__(self, stream, limit):
        self.stream = stream
        self.limit = limit
        self.size = 0

    def write(self, chunk):
        self.stream.write(chunk[:max(0, self.limit - self.size)])
        self.stream.flush()
        self.size += len(chunk)
        if self.size > self.limit:
            raise ValueError("test child output exceeded its combined byte limit")

    def flush(self):
        self.stream.flush()


def run(request_path: Path, result_path: Path):
    if request_path.stat().st_size > MAX_REQUEST:
        raise ValueError("oversized test process request")
    request = json.loads(request_path.read_bytes())
    limit = request["maxOutputBytes"]
    timeout = request["timeoutMs"]
    if type(limit) is not int or not 0 < limit <= MAX_OUTPUT or type(timeout) is not int or not 0 < timeout <= 120_000:
        raise ValueError("invalid test process budgets")
    result = {"settled": True, "supervisor_pid": os.getpid()}
    log_path = request_path.with_suffix(".output.log")
    try:
        with log_path.open("wb") as log:
            completed = run_sample(
                request["command"], cwd=Path(request["cwd"]),
                env={**os.environ, **request["env"]}, timeout=timeout / 1000,
                output_limit=limit, log=CombinedOutput(log, limit),
            )
        result.update(code=completed.returncode, stdout=completed.stdout.decode("utf8", errors="replace"),
                      stderr=completed.stderr.decode("utf8", errors="replace"))
    except BaseException as error:
        with log_path.open("rb") as log:
            evidence = log.read(16 * 1024).decode("utf8", errors="replace")
        result["error"] = f"{type(error).__name__}: {error}"[:4096] + "\n" + evidence
    # Failed evidence is never a successful physical-closure acknowledgement.
    try:
        require_sample_settlement()
    except BaseException as error:
        result["settled"] = False
        result["error"] = result.get("error", "") + "\n" + str(error)[:4096]
    encoded = json.dumps(result, ensure_ascii=True).encode()
    if len(encoded) > MAX_OUTPUT * 6 + 8192:
        raise ValueError("oversized test process result")
    result_path.write_bytes(encoded)
    if not result["settled"]:
        raise SystemExit(125)


if __name__ == "__main__":
    try:
        with ParentLifeline(0):
            run(Path(sys.argv[1]), Path(sys.argv[2]))
    except BaseException:
        # Missing result is an explicit unproven lifetime; caller retains scratch.
        sys.exit(125)
