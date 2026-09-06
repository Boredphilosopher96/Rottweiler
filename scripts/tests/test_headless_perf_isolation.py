from __future__ import annotations

import json
import os
from pathlib import Path
import re
import shutil
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
    def test_candidate_builders_publish_one_attempt_bound_complete_product(self) -> None:
        for filename, platforms in (("ci.yml", ("linux", "macos")),
                                    ("nightly.yml", ("linux", "macos")),
                                    ("performance.yml", ("linux",))):
            workflow = (REPO / ".github/workflows" / filename).read_text()
            for platform in platforms:
                with self.subTest(workflow=filename, platform=platform):
                    build = workflow_job(workflow, platform + "-candidate-build")
                    self.assertNotIn("\n    needs:", build)
                    self.assertNotIn("\n    if:", build)
                    self.assertEqual(build.count("scripts/build-native-candidate.py"), 1)
                    cache = re.search(r'workspaces: "\. -> ([^"]+)"', build)
                    self.assertIsNotNone(cache)
                    relative_target = Path(cache.group(1))
                    self.assertFalse(relative_target.is_absolute())
                    self.assertNotIn("..", relative_target.parts)
                    self.assertNotIn("${{", str(relative_target))
                    self.assertIn(f'--target-dir "$GITHUB_WORKSPACE/{relative_target}"', build)
                    self.assertIn("$RUNNER_TEMP/native-candidates.noindex", build)
                    self.assertIn("--github-output", build)
                    self.assertIn("candidate_artifact:", build)
                    self.assertIn("${{ github.run_id }}-${{ github.run_attempt }}", build)
                    self.assertIn("if-no-files-found: error", build)
                    self.assertNotIn("overwrite: true", build)
                    self.assertNotIn("perf_gate.sh", build)

    def test_native_measurements_keep_platform_provenance_and_fixed_samples(self) -> None:
        for filename, linux_job, macos_job in (("performance.yml", "performance-linux", "performance-macos"),
                                              ("nightly.yml", "linux-release-budget", "macos-release-budget")):
            workflow = (REPO / ".github/workflows" / filename).read_text()
            linux = workflow_job(workflow, linux_job)
            macos = workflow_job(workflow, macos_job)
            self.assertIn("needs: linux-candidate-build", linux)
            self.assertNotIn("macos-candidate-build", linux)
            self.assertIn("runs-on: ubuntu-24.04", linux)
            self.assertIn("native_candidate.py prepare", linux)
            self.assertNotIn("scripts/build-native-candidate.py", linux)
            self.assertNotIn("\n    needs:", macos)
            self.assertNotIn("actions/download-artifact@", macos)
            self.assertIn("runs-on: macos-15", macos)
            self.assertEqual(macos.count("scripts/build-native-candidate.py"), 1)
            self.assertLess(macos.index("scripts/build-native-candidate.py"), macos.index("perf_gate.sh"))
            for measured in (linux, macos):
                self.assertNotIn("runner-contract", measured)
                self.assertNotIn("ROTTWEILER_PERF_PREBUILT_RW", measured)
                self.assertEqual(measured.count("ROTTWEILER_PERF_SAMPLES: 500"), 1)
                self.assertNotIn("build-release.sh", measured)
                self.assertIn("m4_release_gate.sh", measured)
                self.assertIn("timeout-minutes: 60", measured)
            self.assertIn('perf_gate.sh "$RUNNER_TEMP/native-candidate.noindex"', linux)
            self.assertIn('perf_gate.sh "${{ steps.candidate.outputs.candidate }}"', macos)

    def test_release_consumes_preflight_evidence_without_remeasuring(self) -> None:
        release = (REPO / ".github/workflows/release.yml").read_text(encoding="utf-8")
        preflight = (REPO / ".github/workflows/release-preflight.yml").read_text(
            encoding="utf-8"
        )
        contract = workflow_job(release, "runner-contract")
        linux = workflow_job(release, "build-linux")
        macos = workflow_job(release, "build-macos")

        self.assertNotIn("  linux-performance-build:", release)
        self.assertIn("uses: ./.github/workflows/performance.yml", preflight)
        self.assertIn("release-candidate.py verify", contract)
        self.assertIn("actions: read", contract)
        for platform, build, runner in (
            ("linux", linux, "runs-on: ubuntu-24.04"),
            ("macos", macos, "runs-on: macos-15"),
        ):
            with self.subTest(platform=platform):
                self.assertIn("needs: runner-contract", build)
                self.assertIn(runner, build)
                self.assertIn("Build deterministic size-gated archive", build)
                self.assertIn("Attest archive provenance", build)
                self.assertIn("Upload unsigned archive for release signing", build)
                self.assertIn("overwrite: true", build)
                self.assertNotIn("perf_gate.sh", build)
                self.assertNotIn("check-perf-baseline.py", build)
                self.assertNotIn("ROTTWEILER_PERF_", build)

    def test_release_compresses_embedded_wasm_and_never_rewrites_compiled_elf(
        self,
    ) -> None:
        build = (REPO / "packages/js-host/build.ts").read_text(encoding="utf-8")
        runtime = (
            REPO / "packages/tui/src/tree-sitter-runtime.ts"
        ).read_text(encoding="utf-8")
        native_strip = (
            "stripLinuxNativeLibrary(outputNativePath)"
        )
        bundle_gate = "enforceJavaScriptBundleSize(outputExecutable, outputNativePath)"
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
        self.assertIn('"bun-linux-x64-baseline" as const', build)
        self.assertIn("const MAX_RUNTIME_BYTES = 32 * 1024 * 1024", runtime)
        self.assertIn("Linux Bun compiled output bytes:", build)
        self.assertIn(native_strip, build)
        self.assertIn("process.platform === \"darwin\"", build)
        self.assertIn("releasePlatformForNodeTarget", build)
        self.assertIn("productBudgets.jsBundleLessThanBytes", build)
        self.assertNotIn("100_000_000", build)
        self.assertNotIn("150_000_000", build)
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
        self.assertIn("time.sleep(60)", gate)
        self.assertNotIn('sys.platform == "darwin" else 1', gate)
        self.assertIn("for index in range(-5, 0)", gate)
        self.assertNotIn("warmup_count", gate)
        self.assertIn("if smoke and start_p50 >= 80", gate)
        self.assertIn("if smoke and turn_p50 >= 20", gate)
        self.assertIn('protected_start_limit_ms = 200 if sys.platform == "darwin" else 80', gate)
        self.assertIn("protected_turn_limit_ms = 60", gate)
        self.assertIn("if not smoke and start_p99 >= protected_start_limit_ms", gate)
        self.assertIn("if not smoke and turn_p99 >= protected_turn_limit_ms", gate)
        self.assertIn("sample_count > 5000", gate)
        self.assertIn('"samples": [', gate)
        self.assertIn('"runner": {', gate)
        self.assertIn('f"{output.stem}.evidence{output.suffix}"', gate)
        self.assertIn("source_metadata = os.fstat(source.fileno())", gate)
        self.assertIn("source_metadata.st_nlink != 1", gate)
        self.assertIn("from release_contract import load_contract", gate)
        self.assertIn("product_budgets.engine_less_than_bytes", gate)
        self.assertNotIn("40_000_000", gate)
        self.assertNotIn("28_000_000", gate)
        self.assertLess(
            gate.index("evidence_temporary.replace(evidence)"),
            gate.index("if smoke and start_p50 >= 80"),
        )

    def prepared_gate(self, binary_source):
        from test_native_candidate import NativeCandidateTests, packager, native_candidate
        fixture = NativeCandidateTests()
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        for relative in ("crates/rw-cli/tests/perf_gate.sh", "scripts/native_candidate.py",
                         "scripts/artifact_bundle.py", "scripts/release_contract.py", "scripts/perf_process.py"):
            destination = fixture.repo / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(REPO / relative, destination)
        gate = fixture.repo / "crates/rw-cli/tests/perf_gate.sh"
        binary = fixture.stage / "bin/rw"
        binary.write_text(binary_source)
        packager.package(fixture.stage, fixture.archive, 1700000000)
        fixture.identity["source"] = native_candidate.source_identity(fixture.repo)
        fixture.publish()
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
            "ROTTWEILER_PERF_SAMPLES": "100",
            "RUNNER_TEMP": str(root),
        }
        return fixture, gate, output, env

    def test_prebuilt_gate_keeps_metrics_schema_and_writes_ordered_evidence(self) -> None:
        from test_native_candidate import native_candidate
        fixture, gate, output, env = self.prepared_gate(
            "#!/bin/sh\nprintf 'ready\\n'\nprintf 'rw_perf_zero_latency_turn_us=100\\n' >&2\n"
        )
        subprocess.run([str(gate), str(fixture.root)], cwd=fixture.repo, env=env, check=True)
        metrics = json.loads(output.read_text())
        self.assertEqual(set(metrics), {"schema_version", "metrics"})
        evidence = json.loads(output.with_name("headless.evidence.json").read_text())
        self.assertEqual(set(evidence), {
            "schema_version", "sample_count", "samples", "runner", "candidate", "status", "phase", "error"
        })
        self.assertEqual(evidence["candidate"]["engine_sha256"], native_candidate.hash_file(fixture.stage / "bin/rw"))
        self.assertEqual(evidence["sample_count"], 100)
        self.assertEqual(evidence["status"], "pass")
        self.assertEqual(evidence["phase"], "complete")
        self.assertIsNone(evidence["error"])
        self.assertEqual([sample["index"] for sample in evidence["samples"]], list(range(100)))
        self.assertTrue(all(set(sample) == {"index", "headless_print_us", "turn_overhead_us"}
                            for sample in evidence["samples"]))

    def test_invalid_sample_retains_prior_observations_and_failing_phase(self):
        fixture, gate, output, env = self.prepared_gate(
            "#!/bin/sh\nprintf 'ready\\n'\n"
            "case \"$HOME\" in *home-2) printf 'rw_perf_zero_latency_turn_us=-1\\n' >&2;;\n"
            "*) printf 'rw_perf_zero_latency_turn_us=100\\n' >&2;; esac\n"
        )
        run = subprocess.run([str(gate), str(fixture.root)], cwd=fixture.repo, env=env, capture_output=True)
        self.assertNotEqual(run.returncode, 0)
        self.assertFalse(output.exists())
        evidence = json.loads(output.with_name("headless.evidence.json").read_text())
        self.assertEqual(evidence["status"], "fail")
        self.assertEqual(evidence["phase"], "sampling")
        self.assertIn("invalid or duplicate performance marker", evidence["error"])
        self.assertEqual([sample["index"] for sample in evidence["samples"]], [0, 1])
        self.assertEqual(evidence["candidate"]["source"], fixture.identity["source"])


if __name__ == "__main__":
    unittest.main()
