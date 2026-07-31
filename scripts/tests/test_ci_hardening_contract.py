import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


def workflow_job(workflow: str, name: str) -> str:
    match = re.search(
        rf"(?ms)^  {re.escape(name)}:\n(.*?)(?=^  [a-z0-9][a-z0-9-]*:\n|\Z)",
        workflow,
    )
    if match is None:
        raise AssertionError(f"workflow job {name!r} is missing")
    return match.group(1)


class CiHardeningContractTests(unittest.TestCase):
    def assert_checkout_credentials_are_not_persisted(self, workflow: str) -> None:
        marker = "uses: actions/checkout@"
        checkouts = workflow.split(marker)[1:]
        self.assertTrue(checkouts)
        for checkout in checkouts:
            checkout_header = "\n".join(checkout.splitlines()[:4])
            self.assertIn("persist-credentials: false", checkout_header)

    def test_pull_request_ci_runs_bounded_headless_and_tui_performance_smoke(self) -> None:
        workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assert_checkout_credentials_are_not_persisted(workflow)
        smoke = workflow.split("  performance-smoke:", 1)[1].split(
            "  m4-ssh-loopback:", 1
        )[0]
        self.assertNotIn("workflow_dispatch", workflow)
        self.assertNotIn("performance-runner-contract", workflow)
        self.assertNotIn("linux-performance-build", workflow)
        self.assertNotIn("macos-performance-build", workflow)
        self.assertNotIn("performance-linux", workflow)
        self.assertNotIn("performance-macos", workflow)
        self.assertIn("needs: [test, security-tests]", smoke)
        self.assertNotIn("\n    if:", smoke)
        self.assertIn("timeout-minutes: 45", smoke)
        self.assertIn("Swatinem/rust-cache@c19371144df3bb44fab255c43d04cbc2ab54d1c4", smoke)
        self.assertIn("ROTTWEILER_PERF_SMOKE: 1", smoke)
        self.assertIn("ROTTWEILER_PERF_SAMPLES: 100", smoke)
        self.assertIn("bun run test:perf", smoke)

    def test_nightly_budgets_are_blocking_and_missing_runners_fail_closed(self) -> None:
        workflow = (ROOT / ".github/workflows/nightly.yml").read_text(encoding="utf-8")
        self.assert_checkout_credentials_are_not_persisted(workflow)
        linux_release = workflow.split("  linux-release-budget:", 1)[1].split(
            "  macos-release-budget:", 1
        )[0]
        macos_release = workflow.split("  macos-release-budget:", 1)[1].split(
            "  eight-hour-soak:", 1
        )[0]
        runner_contract = workflow.split("  runner-contract:", 1)[1].split(
            "  fuzz:", 1
        )[0]
        self.assertNotIn("continue-on-error", linux_release + macos_release)
        self.assertIn(
            "needs: [runner-contract, linux-performance-build]", linux_release
        )
        self.assertNotIn("macos-performance-build", linux_release)
        self.assertIn(
            "needs: [runner-contract, macos-performance-build]", macos_release
        )
        self.assertIn(
            "runs-on: [self-hosted, Linux, X64, performance]", linux_release
        )
        self.assertIn(
            "runs-on: [self-hosted, macOS, ARM64, performance]", macos_release
        )
        self.assertIn("--platform linux-x86_64", linux_release)
        self.assertIn("--platform darwin-arm64", macos_release)
        for release_budget in (linux_release, macos_release):
            self.assertIn("--require-measured", release_budget)
            self.assertIn("scripts/build-release.sh", release_budget)
        self.assertIn("ROTTWEILER_SELF_HOSTED_RUNNERS", runner_contract)
        self.assertIn("exit 1", runner_contract)
        self.assertEqual(workflow.count("runner-contract"), 6)
        for job_name in ("eight-hour-soak", "wsl2-acceptance", "terminal-bench"):
            match = re.search(
                rf"(?ms)^  {re.escape(job_name)}:\n(.*?)(?=^  [a-z0-9][a-z0-9-]*:\n|\Z)",
                workflow,
            )
            self.assertIsNotNone(match)
            self.assertIn("needs: runner-contract", match.group(1))
        terminal_bench = workflow.split("  terminal-bench:", 1)[1]
        job_environment = terminal_bench.split("    steps:", 1)[0]
        run_step = terminal_bench.split("      - name: Run pinned 20-task subset", 1)[1].split(
            "      - name:", 1
        )[0]
        self.assertNotIn("secrets.ROTTWEILER_EVAL_API_KEY", job_environment)
        self.assertIn("secrets.ROTTWEILER_EVAL_API_KEY", run_step)

    def test_protected_performance_consumers_fail_closed_before_queueing(self) -> None:
        performance = (ROOT / ".github/workflows/performance.yml").read_text(
            encoding="utf-8"
        )
        nightly = (ROOT / ".github/workflows/nightly.yml").read_text(encoding="utf-8")
        release = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        self.assert_checkout_credentials_are_not_persisted(performance)
        self.assertIn("on:\n  workflow_dispatch:", performance)
        self.assertNotIn("pull_request:", performance)
        self.assertNotIn("\n  push:", performance)
        self.assertNotIn("github.event_name", performance)

        cases = (
            (
                performance,
                "runner-contract",
                ("performance-linux", "performance-macos"),
            ),
            (
                nightly,
                "runner-contract",
                ("linux-release-budget", "macos-release-budget"),
            ),
            (
                release,
                "runner-contract",
                ("build-linux", "build-macos"),
            ),
        )
        for workflow, contract_name, consumer_names in cases:
            contract = workflow_job(workflow, contract_name)
            with self.subTest(contract=contract_name):
                self.assertIn("runs-on: ubuntu-latest", contract)
                self.assertIn("timeout-minutes: 5", contract)
                self.assertIn("vars.ROTTWEILER_SELF_HOSTED_RUNNERS", contract)
                self.assertIn("exit 1", contract)
            for consumer_name in consumer_names:
                consumer = workflow_job(workflow, consumer_name)
                with self.subTest(consumer=consumer_name):
                    self.assertIn(contract_name, consumer.split("    runs-on:", 1)[0])

    def test_release_preflight_reuses_protected_performance_and_stays_non_publishing(
        self,
    ) -> None:
        preflight = (ROOT / ".github/workflows/release-preflight.yml").read_text(
            encoding="utf-8"
        )
        performance = (ROOT / ".github/workflows/performance.yml").read_text(
            encoding="utf-8"
        )
        self.assert_checkout_credentials_are_not_persisted(preflight)
        self.assertIn("workflow_call:", performance)
        self.assertIn("environment: release", preflight)
        self.assertIn("python3 scripts/check-release-readiness.py", preflight)
        self.assertIn("python3 scripts/check-dogfood-gate.py", preflight)
        self.assertIn("uses: ./.github/workflows/performance.yml", preflight)
        self.assertIn("needs: repository-prerequisites", preflight)
        self.assertNotIn("gh release create", preflight)
        self.assertNotIn("git push", preflight)
        self.assertNotIn("HOMEBREW_TAP_TOKEN:", preflight.split("    steps:", 1)[0])

    def test_quality_workflow_pins_coverage_and_mutation_tools(self) -> None:
        workflow = (ROOT / ".github/workflows/quality.yml").read_text(
            encoding="utf-8"
        )
        self.assert_checkout_credentials_are_not_persisted(workflow)
        self.assertIn("cargo-llvm-cov --version 0.8.7", workflow)
        self.assertIn("cargo-mutants --version 27.1.0", workflow)
        self.assertIn("cargo llvm-cov", workflow)
        self.assertIn("cargo mutants", workflow)
        self.assertIn("--jobs 2", workflow)
        self.assertNotIn("--in-place", workflow)
        self.assertEqual(
            workflow.count(
                "oven-sh/setup-bun@0c5077e51419868618aeaa5fe8019c62421857d6"
            ),
            2,
        )
        self.assertEqual(
            workflow.count("bun install --cwd packages/plugin-sdk --frozen-lockfile"),
            2,
        )
        self.assertEqual(
            workflow.count("bun install --cwd packages/tui --frozen-lockfile"),
            2,
        )
        for boundary in (
            "crates/rw-core/src/permission.rs",
            "crates/rw-store/src/trust.rs",
            "crates/rw-core/src/update.rs",
            "crates/rw-ext/src/plugin.rs",
        ):
            self.assertIn(boundary, workflow)

    def test_dependabot_covers_every_ecosystem_without_hiding_major_upgrades(
        self,
    ) -> None:
        configuration = (ROOT / ".github/dependabot.yml").read_text(encoding="utf-8")
        self.assertEqual(configuration.count("package-ecosystem:"), 5)
        self.assertNotIn("version-update:semver-major", configuration)

    def test_signed_release_is_serialized_and_downloads_only_unsigned_archives(
        self,
    ) -> None:
        workflow = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        sign_and_publish = workflow.split("  sign-and-publish:", 1)[1]
        attest_pin = (
            "actions/attest@f7c74d28b9d84cb8768d0b8ca14a4bac6ef463e6 # v4.2.0"
        )

        self.assertIn(
            "concurrency:\n  group: signed-release\n  cancel-in-progress: false",
            workflow,
        )
        self.assertEqual(workflow.count(attest_pin), 3)
        self.assertNotIn("actions/attest@f6bf1532", workflow)
        self.assertEqual(sign_and_publish.count("name: release-darwin-arm64"), 1)
        self.assertEqual(sign_and_publish.count("name: release-linux-x86_64"), 1)
        self.assertNotIn("pattern: release-*", sign_and_publish)
        self.assertNotIn("merge-multiple:", sign_and_publish)

    def test_release_provider_and_eval_secrets_are_step_scoped(self) -> None:
        workflow = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        release_gate = workflow_job(workflow, "release-gate")
        release_gate_environment = release_gate.split("    steps:", 1)[0]
        paid_step = release_gate.split(
            "      - name: Paid two-family record and network-denied replay canary", 1
        )[1].split("      - name:", 1)[0]
        terminal_bench = workflow_job(workflow, "release-terminal-bench")
        terminal_job_environment = terminal_bench.split("    steps:", 1)[0]
        eval_step = terminal_bench.split(
            "      - name: Run exact release archive through the pinned 20-task capability gate",
            1,
        )[1].split("      - name:", 1)[0]

        for secret in ("secrets.OPENAI_API_KEY", "secrets.ANTHROPIC_API_KEY"):
            self.assertNotIn(secret, release_gate_environment)
            self.assertIn(secret, paid_step)
        self.assertNotIn(
            "secrets.ROTTWEILER_EVAL_API_KEY", terminal_job_environment
        )
        self.assertIn("secrets.ROTTWEILER_EVAL_API_KEY", eval_step)

    def test_rerun_artifacts_preserve_producers_and_version_evidence(self) -> None:
        workflows = {
            name: (ROOT / f".github/workflows/{name}.yml").read_text(encoding="utf-8")
            for name in ("performance", "nightly", "release")
        }

        for workflow_name, workflow in workflows.items():
            for platform in ("linux", "macos"):
                with self.subTest(workflow=workflow_name, producer=platform):
                    producer = workflow_job(
                        workflow, f"{platform}-performance-build"
                    )
                    self.assertIn(
                        f"name: {platform}-performance-rw-${{{{ github.run_id }}}}",
                        producer,
                    )
                    self.assertNotIn("github.run_attempt", producer)
                    self.assertIn("overwrite: true", producer)

        release = workflows["release"]
        for platform in ("linux-x86_64", "darwin-arm64"):
            with self.subTest(release_archive=platform):
                build = workflow_job(
                    release,
                    "build-linux" if platform == "linux-x86_64" else "build-macos",
                )
                self.assertIn(f"name: release-{platform}", build)
                self.assertIn("overwrite: true", build)

        expected_evidence_names = {
            "performance": (
                "manual-performance-linux-x86_64-${{ github.run_id }}-${{ github.run_attempt }}",
                "manual-performance-darwin-arm64-${{ github.run_id }}-${{ github.run_attempt }}",
            ),
            "nightly": (
                "nightly-performance-linux-x86_64-${{ github.run_id }}-${{ github.run_attempt }}",
                "nightly-performance-darwin-arm64-${{ github.run_id }}-${{ github.run_attempt }}",
                "soak-${{ matrix.platform }}-${{ github.run_id }}-${{ github.run_attempt }}",
                "terminal-bench-${{ github.run_id }}-${{ github.run_attempt }}",
            ),
            "release": (
                "performance-linux-x86_64-${{ github.ref_name }}-${{ github.run_attempt }}",
                "performance-darwin-arm64-${{ github.ref_name }}-${{ github.run_attempt }}",
                "release-gate-evidence-${{ github.ref_name }}-${{ github.run_attempt }}",
                "release-terminal-bench-${{ github.ref_name }}-${{ github.run_attempt }}",
                "release-soak-${{ matrix.platform }}-${{ github.ref_name }}-${{ github.run_attempt }}",
            ),
        }
        for workflow_name, evidence_names in expected_evidence_names.items():
            for artifact_name in evidence_names:
                with self.subTest(workflow=workflow_name, evidence=artifact_name):
                    self.assertIn(f"name: {artifact_name}", workflows[workflow_name])


if __name__ == "__main__":
    unittest.main()
