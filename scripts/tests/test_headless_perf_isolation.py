from __future__ import annotations

import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[2]


def workflow_job(workflow: str, name: str) -> str:
    match = re.search(
        rf"(?ms)^  {re.escape(name)}:\n(.*?)(?=^  [a-z0-9][a-z0-9-]*:\n|\Z)",
        workflow,
    )
    if match is None:
        raise AssertionError(f"workflow job {name!r} is missing")
    return match.group(1)


class HeadlessPerformanceIsolationTests(unittest.TestCase):
    def test_platform_builders_use_noindex_runner_temp_and_checksum_binary(
        self,
    ) -> None:
        cases = [
            ("linux", "Linux", "sha256sum rw > rw.sha256"),
            ("macos", "Darwin", "shasum -a 256 rw > rw.sha256"),
        ]
        for platform, uname, checksum in cases:
            with self.subTest(platform=platform):
                builder = (
                    REPO / f"scripts/prepare-{platform}-performance-binary.sh"
                ).read_text(encoding="utf-8")

                self.assertIn(f'if [ "$(uname -s)" != {uname} ]', builder)
                self.assertIn(
                    f"$RUNNER_TEMP/rottweiler-{platform}-performance-build.noindex",
                    builder,
                )
                self.assertIn(
                    f"$RUNNER_TEMP/rottweiler-{platform}-performance-artifact.noindex",
                    builder,
                )
                self.assertIn('CARGO_TARGET_DIR="$build_root"', builder)
                self.assertIn("install -m 700", builder)
                self.assertIn(checksum, builder)

    def test_ci_builds_platform_binaries_on_isolated_runners(self) -> None:
        workflow = (REPO / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        linux_build = workflow_job(workflow, "linux-performance-build")
        macos_build = workflow_job(workflow, "macos-performance-build")

        for platform, build in (("linux", linux_build), ("macos", macos_build)):
            with self.subTest(platform=platform):
                self.assertIn(
                    "needs: [test, security-tests, performance-smoke]", build
                )
                self.assertIn(
                    f"scripts/prepare-{platform}-performance-binary.sh", build
                )
                self.assertIn("actions/upload-artifact@043fb46d", build)
                self.assertIn("if-no-files-found: error", build)
                self.assertIn("overwrite: true", build)
                self.assertIn("timeout-minutes: 30", build)

    def test_ci_performance_consumers_are_platform_independent(self) -> None:
        workflow = (REPO / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        cases = (
            (
                "linux",
                workflow_job(workflow, "performance-linux"),
                "linux-performance-build",
                "macos-performance-build",
                "runs-on: [self-hosted, Linux, X64, performance]",
                "sha256sum -c rw.sha256",
                "Headless performance gate (Linux prebuilt binary)",
                "pr-performance-linux-x86_64-${{ github.run_id }}-${{ github.run_attempt }}",
            ),
            (
                "macos",
                workflow_job(workflow, "performance-macos"),
                "macos-performance-build",
                "linux-performance-build",
                "runs-on: [self-hosted, macOS, ARM64, performance]",
                "shasum -a 256 -c rw.sha256",
                "Headless performance gate (macOS prebuilt binary)",
                "pr-performance-darwin-arm64-${{ github.run_id }}-${{ github.run_attempt }}",
            ),
        )
        for (
            platform,
            performance,
            builder,
            other_builder,
            runner,
            checksum,
            gate,
            evidence,
        ) in cases:
            with self.subTest(platform=platform):
                self.assertIn(builder, performance)
                self.assertNotIn(other_builder, performance)
                self.assertIn("performance-runner-contract", performance)
                self.assertIn(runner, performance)
                self.assertIn("actions/download-artifact@37930b1c", performance)
                self.assertIn(checksum, performance)
                self.assertEqual(performance.count("ROTTWEILER_PERF_PREBUILT_RW:"), 1)
                self.assertEqual(performance.count("ROTTWEILER_PERF_SAMPLES: 500"), 1)
                self.assertIn(gate, performance)
                self.assertNotIn("Headless performance gate (Linux source build)", performance)
                self.assertIn(evidence, performance)
                self.assertIn("timeout-minutes: 60", performance)
                self.assertLess(
                    performance.index(gate),
                    performance.index("Install Rust toolchain"),
                )

    def test_nightly_reuses_isolated_platform_measurements_independently(
        self,
    ) -> None:
        nightly = (REPO / ".github/workflows/nightly.yml").read_text(encoding="utf-8")
        linux_builder = nightly.split("  linux-performance-build:", 1)[1].split(
            "  macos-performance-build:", 1
        )[0]
        macos_builder = nightly.split("  macos-performance-build:", 1)[1].split(
            "  linux-release-budget:", 1
        )[0]
        linux = nightly.split("  linux-release-budget:", 1)[1].split(
            "  macos-release-budget:", 1
        )[0]
        macos = nightly.split("  macos-release-budget:", 1)[1].split(
            "  eight-hour-soak:", 1
        )[0]

        self.assertIn("scripts/prepare-linux-performance-binary.sh", linux_builder)
        self.assertIn("scripts/prepare-macos-performance-binary.sh", macos_builder)
        self.assertIn("actions/upload-artifact@043fb46d", linux_builder)
        self.assertIn("actions/upload-artifact@043fb46d", macos_builder)
        self.assertIn("needs: [runner-contract, linux-performance-build]", linux)
        self.assertNotIn("macos-performance-build", linux)
        self.assertIn("needs: [runner-contract, macos-performance-build]", macos)
        self.assertNotIn("linux-performance-build", macos)
        self.assertIn("runs-on: [self-hosted, Linux, X64, performance]", linux)
        self.assertIn("runs-on: [self-hosted, macOS, ARM64, performance]", macos)
        self.assertIn("sha256sum -c rw.sha256", linux)
        self.assertIn("shasum -a 256 -c rw.sha256", macos)
        self.assertIn("Headless performance gate (Linux prebuilt binary)", linux)
        self.assertIn("Headless performance gate (macOS prebuilt binary)", macos)
        self.assertNotIn("Headless performance gate (Linux source build)", linux)
        for measured in (linux, macos):
            self.assertEqual(measured.count("ROTTWEILER_PERF_PREBUILT_RW:"), 1)
            self.assertEqual(measured.count("ROTTWEILER_PERF_SAMPLES: 500"), 1)
            self.assertLess(
                measured.index("Headless performance gate"),
                measured.index("Install Rust toolchain"),
            )

    def test_release_reuses_isolated_platform_measurements_independently(self) -> None:
        release = (REPO / ".github/workflows/release.yml").read_text(encoding="utf-8")
        linux_builder = workflow_job(release, "linux-performance-build")
        macos_builder = workflow_job(release, "macos-performance-build")
        linux = workflow_job(release, "build-linux")
        macos = workflow_job(release, "build-macos")

        self.assertIn("scripts/prepare-linux-performance-binary.sh", linux_builder)
        self.assertIn("scripts/prepare-macos-performance-binary.sh", macos_builder)
        for platform, build, builder, other_builder, runner, checksum, gate in (
            (
                "linux",
                linux,
                "linux-performance-build",
                "macos-performance-build",
                "runs-on: [self-hosted, Linux, X64, performance]",
                "sha256sum -c rw.sha256",
                "Headless performance gate (Linux prebuilt binary)",
            ),
            (
                "macos",
                macos,
                "macos-performance-build",
                "linux-performance-build",
                "runs-on: [self-hosted, macOS, ARM64, performance]",
                "shasum -a 256 -c rw.sha256",
                "Headless performance gate (macOS prebuilt binary)",
            ),
        ):
            with self.subTest(platform=platform):
                self.assertIn(f"needs: [runner-contract, {builder}]", build)
                self.assertNotIn(other_builder, build)
                self.assertIn(runner, build)
                self.assertIn(checksum, build)
                self.assertIn(gate, build)
                self.assertNotIn("Headless performance gate (Linux source build)", build)
                self.assertEqual(build.count("ROTTWEILER_PERF_PREBUILT_RW:"), 1)
                self.assertEqual(build.count("ROTTWEILER_PERF_SAMPLES: 500"), 1)
                self.assertIn("timeout-minutes: 60", build)
                self.assertLess(build.index(gate), build.index("Install Rust toolchain"))
                self.assertIn("overwrite: true", build)

    def test_release_compresses_embedded_wasm_and_never_rewrites_compiled_elf(
        self,
    ) -> None:
        build = (REPO / "packages/tui/build.ts").read_text(encoding="utf-8")
        runtime = (
            REPO / "packages/tui/src/tree-sitter-runtime.ts"
        ).read_text(encoding="utf-8")
        native_strip = (
            "stripLinuxNativeLibrary(outputNativePath)"
        )
        bundle_gate = "enforceTuiBundleSize(outputExecutable, outputNativePath)"
        embedded_smoke = "compiled embedded-parser smoke failed"

        self.assertIn('name: "rottweiler-compressed-tree-sitter-assets"', build)
        self.assertIn('build.onLoad({ filter: /\\.(?:wasm|scm)$/ }', build)
        self.assertIn(
            'build.onLoad({ filter: /(?:parser\\.worker|tree-sitter)\\.js$/ }',
            build,
        )
        self.assertIn(
            "Bun.zstdCompressSync(source, { level: 19 })",
            build,
        )
        self.assertIn("plugins: [compressedTreeSitterAssets, nativePrelude]", build)
        self.assertIn("bytecode: true", build)
        self.assertIn("new DataView(contents.buffer).setUint32(4, source.byteLength, true)", build)
        self.assertIn("expectedBytes > MAX_ASSET_BYTES", runtime)
        self.assertIn("Bun.zstdDecompressSync(compressed)", runtime)
        self.assertIn("bytes.byteLength !== expectedBytes", runtime)
        self.assertNotIn("maxOutputLength", runtime)
        self.assertIn('target: "bun-linux-x64-baseline" as const', build)
        self.assertIn("const MAX_RUNTIME_BYTES = 32 * 1024 * 1024", runtime)
        self.assertIn("Linux Bun compiled output bytes:", build)
        self.assertIn(native_strip, build)
        self.assertIn("process.platform === \"darwin\"", build)
        self.assertIn("? 100_000_000", build)
        self.assertIn("? 110_000_000", build)
        self.assertIn(bundle_gate, build)
        self.assertNotIn("executablePath:", build)
        self.assertNotIn("stripLinuxArtifact", build)
        self.assertNotIn("objcopy", build)
        self.assertLess(build.index(native_strip), build.index(bundle_gate))
        self.assertLess(build.index(bundle_gate), build.index(embedded_smoke))

    def test_gate_preserves_fixed_sampling_and_writes_evidence_first(self) -> None:
        gate = (REPO / "crates/rw-cli/tests/perf_gate.sh").read_text(
            encoding="utf-8"
        )

        self.assertIn('smoke = os.environ.get("ROTTWEILER_PERF_SMOKE") == "1"', gate)
        self.assertIn('"100" if smoke else "500"', gate)
        self.assertIn("minimum_samples = 100", gate)
        self.assertIn('60 if sys.platform == "darwin" else 1', gate)
        self.assertIn("for index in range(-5, 0)", gate)
        self.assertNotIn("warmup_count", gate)
        self.assertIn("if smoke and start_p50 >= 80", gate)
        self.assertIn("if smoke and turn_p50 >= 20", gate)
        self.assertIn("if not smoke and start_p99 >= 80", gate)
        self.assertIn("if not smoke and turn_p99 >= 20", gate)
        self.assertIn("sample_count > 5000", gate)
        self.assertIn('"samples": [', gate)
        self.assertIn('"runner": {', gate)
        self.assertIn('f"{output.stem}.evidence{output.suffix}"', gate)
        self.assertIn("source_metadata = os.fstat(source.fileno())", gate)
        self.assertIn("source_metadata.st_nlink != 1", gate)
        self.assertLess(
            gate.index("evidence_temporary.replace(evidence)"),
            gate.index("if smoke and start_p50 >= 80"),
        )

    def test_prebuilt_gate_keeps_metrics_schema_and_writes_ordered_evidence(self) -> None:
        gate = REPO / "crates/rw-cli/tests/perf_gate.sh"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "rw"
            binary.write_text(
                "#!/bin/sh\n"
                "printf 'ready\\n'\n"
                "printf 'rw_perf_zero_latency_turn_us=1000\\n' >&2\n",
                encoding="utf-8",
            )
            binary.chmod(0o700)
            site = root / "site"
            site.mkdir()
            (site / "sitecustomize.py").write_text(
                "import time\ntime.sleep = lambda _seconds: None\n", encoding="utf-8"
            )
            output = root / "results" / "headless.json"
            env = {
                **os.environ,
                "GITHUB_ACTIONS": "true",
                "PYTHONPATH": str(site),
                "ROTTWEILER_PERF_OUTPUT": str(output),
                "ROTTWEILER_PERF_PREBUILT_RW": str(binary),
                "ROTTWEILER_PERF_SAMPLES": "100",
                "RUNNER_TEMP": str(root),
            }

            subprocess.run([str(gate)], cwd=REPO, env=env, check=True)

            metrics = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(set(metrics), {"schema_version", "metrics"})
            evidence_path = output.with_name("headless.evidence.json")
            evidence = json.loads(evidence_path.read_text(encoding="utf-8"))
            self.assertEqual(
                set(evidence), {"schema_version", "sample_count", "samples", "runner"}
            )
            self.assertEqual(evidence["sample_count"], 100)
            self.assertEqual(
                [sample["index"] for sample in evidence["samples"]], list(range(100))
            )
            self.assertTrue(
                all(
                    set(sample)
                    == {"index", "headless_print_us", "turn_overhead_us"}
                    for sample in evidence["samples"]
                )
            )


if __name__ == "__main__":
    unittest.main()
