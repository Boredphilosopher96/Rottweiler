#!/usr/bin/env python3
"""Build and compare the release engine at opt-level z and 3.

The experiment holds thin LTO and 16 codegen units constant, alternates the
measurement order to reduce host drift, and exercises the same deterministic
headless and authenticated engine-ready paths as the release gates.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import math
import os
import pathlib
import shutil
import statistics
import subprocess
import sys
import tempfile
import time


def load_m4_gate(repo: pathlib.Path):
    path = repo / "crates/rw-cli/tests/m4_release_gate.py"
    spec = importlib.util.spec_from_file_location("rottweiler_m4_gate", path)
    if spec is None or spec.loader is None:
        raise RuntimeError("could not load the M4 release gate")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def percentile(values: list[float], quantile: float) -> float:
    ordered = sorted(values)
    return ordered[math.ceil(len(ordered) * quantile) - 1]


def build(repo: pathlib.Path, target_root: pathlib.Path, opt_level: str) -> pathlib.Path:
    host = subprocess.check_output(["rustc", "-vV"], text=True).split("host: ", 1)[1].splitlines()[0]
    env = os.environ.copy()
    env.update(
        {
            "CARGO_TARGET_DIR": str(target_root),
            "CARGO_PROFILE_RELEASE_OPT_LEVEL": opt_level,
            "CARGO_PROFILE_RELEASE_LTO": "thin",
            "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "16",
        }
    )
    subprocess.run(
        [
            "cargo",
            "build",
            "--locked",
            "--release",
            "--target",
            host,
            "-p",
            "rw-cli",
            "--bin",
            "rw",
        ],
        cwd=repo,
        env=env,
        check=True,
    )
    return target_root / host / "release/rw"


def headless_sample(binary: pathlib.Path, repo: pathlib.Path, root: pathlib.Path, index: str) -> float:
    home = root / f"headless-{index}"
    home.mkdir(mode=0o700)
    env = {
        "HOME": str(home),
        "ROTTWEILER_HOME": str(home),
        "ROTTWEILER_CREDENTIAL_BACKEND": "file",
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
    }
    started = time.perf_counter_ns()
    run = subprocess.run(
        [
            str(binary),
            "-p",
            "perf",
            "--permission-mode",
            "yolo",
            "--in-memory-replay-script",
            str(repo / "crates/rw-cli/tests/fixtures/perf-script.json"),
            "--output-format",
            "text",
            "--perf-markers",
        ],
        cwd=repo,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    elapsed = (time.perf_counter_ns() - started) / 1_000_000
    if run.returncode != 0 or run.stdout.strip() != b"ready":
        raise RuntimeError(
            f"headless sample failed for {binary}: rc={run.returncode} "
            f"stderr={run.stderr.decode(errors='replace')[-2000:]}"
        )
    return elapsed


def summarize(values: list[float]) -> dict[str, int]:
    return {
        "p50_us": math.ceil(statistics.median(values) * 1000),
        "p95_us": math.ceil(percentile(values, 0.95) * 1000),
        "p99_us": math.ceil(percentile(values, 0.99) * 1000),
        "max_us": math.ceil(max(values) * 1000),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", type=pathlib.Path, default=pathlib.Path(__file__).parents[1])
    parser.add_argument("--samples", type=int, default=100)
    parser.add_argument("--output", type=pathlib.Path)
    args = parser.parse_args()
    if args.samples < 100:
        raise RuntimeError("the release optimization comparison requires at least 100 samples")
    repo = args.repo.resolve()
    m4 = load_m4_gate(repo)
    results: dict[str, dict[str, object]] = {
        "z": {"engine_ready_ms": [], "headless_print_ms": []},
        "3": {"engine_ready_ms": [], "headless_print_ms": []},
    }
    with tempfile.TemporaryDirectory(prefix="rw-opt-", dir="/tmp") as temporary:
        root = pathlib.Path(temporary)
        root.chmod(0o700)
        build_root = root / "target"
        binaries: dict[str, pathlib.Path] = {}
        for opt_level in ["z", "3"]:
            built = build(repo, build_root, opt_level)
            installed = root / f"rw-opt-{opt_level}"
            shutil.copyfile(built, installed)
            installed.chmod(0o700)
            binaries[opt_level] = installed
            results[opt_level]["binary_bytes"] = installed.stat().st_size

        workspace = root / "workspace"
        workspace.mkdir(mode=0o700)
        with m4.fixture_origin() as port:
            for warmup in range(5):
                for opt_level in (["z", "3"] if warmup % 2 == 0 else ["3", "z"]):
                    binary = binaries[opt_level]
                    headless_sample(binary, repo, root, f"warm-{warmup}-{opt_level}")
                    sample_root = root / f"engine-warm-{warmup}-{opt_level}"
                    sample_root.mkdir(mode=0o700)
                    runtime, _ = m4.start_engine(
                        binary,
                        sample_root,
                        workspace,
                        port,
                        f"opt-warm-{warmup}-{opt_level}",
                    )
                    m4.stop_runtime(runtime)
            for index in range(args.samples):
                order = ["z", "3"] if index % 2 == 0 else ["3", "z"]
                for opt_level in order:
                    binary = binaries[opt_level]
                    headless = headless_sample(binary, repo, root, f"{index}-{opt_level}")
                    sample_root = root / f"engine-{index}-{opt_level}"
                    sample_root.mkdir(mode=0o700)
                    runtime = None
                    try:
                        runtime, ready = m4.start_engine(
                            binary,
                            sample_root,
                            workspace,
                            port,
                            f"opt-{index}-{opt_level}",
                        )
                    finally:
                        if runtime is not None:
                            m4.stop_runtime(runtime)
                    results[opt_level]["headless_print_ms"].append(headless)
                    results[opt_level]["engine_ready_ms"].append(ready)

    profiles = {}
    for opt_level, result in results.items():
        profiles[opt_level] = {
            "binary_bytes": result["binary_bytes"],
            "engine_ready": summarize(result["engine_ready_ms"]),
            "headless_print": summarize(result["headless_print_ms"]),
        }
    document = {
        "schema_version": 1,
        "samples": args.samples,
        "controlled_profile": {"lto": "thin", "codegen_units": 16},
        "profiles": profiles,
    }
    encoded = json.dumps(document, indent=2, sort_keys=True) + "\n"
    if args.output is not None:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded, encoding="utf-8")
    sys.stdout.write(encoded)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
