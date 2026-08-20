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
    def test_external_actions_use_consistent_immutable_pins(self) -> None:
        pins: dict[str, str] = {}
        for workflow_path in sorted((ROOT / ".github/workflows").glob("*.yml")):
            workflow = workflow_path.read_text(encoding="utf-8")
            for line_number, line in enumerate(workflow.splitlines(), start=1):
                uses = re.search(r"\buses:\s+(\S+)", line)
                if uses is None or uses.group(1).startswith("./"):
                    continue
                reference = uses.group(1)
                pin = re.fullmatch(r"([^@]+)@([0-9a-f]{40})", reference)
                self.assertIsNotNone(
                    pin,
                    f"{workflow_path.name}:{line_number} must pin an external action to a full commit SHA",
                )
                self.assertIn(
                    " # v",
                    line,
                    f"{workflow_path.name}:{line_number} must document the pinned action version",
                )
                if pin is None:
                    continue
                action, sha = pin.groups()
                previous = pins.setdefault(action, sha)
                self.assertEqual(
                    previous,
                    sha,
                    f"{action} must use one reviewed SHA across all workflows",
                )
        self.assertTrue(pins)

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
        self.assertIn("Swatinem/rust-cache@", smoke)
        self.assertIn("ROTTWEILER_PERF_SMOKE: 1", smoke)
        self.assertIn("ROTTWEILER_PERF_SAMPLES: 100", smoke)
        self.assertIn("bun run test:perf", smoke)
        tui_smoke = workflow_job(workflow, "tui-performance-smoke")
        self.assertIn("ROTTWEILER_PERF_SMOKE: 1", tui_smoke)
        self.assertIn("bun run test:perf", tui_smoke)

    def test_nightly_budgets_are_blocking_and_missing_runners_fail_closed(self) -> None:
        workflow = (ROOT / ".github/workflows/nightly.yml").read_text(encoding="utf-8")
        self.assert_checkout_credentials_are_not_persisted(workflow)
        self.assertIn('cron: "17 5 * * 1"', workflow)
        self.assertNotIn('cron: "17 5 * * *"', workflow)
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
        self.assertIn("needs: runner-contract", macos_release)
        self.assertIn(
            "runs-on: ubuntu-24.04", linux_release
        )
        self.assertIn("runs-on: macos-15", macos_release)
        self.assertNotIn("self-hosted", macos_release)
        self.assertIn("--platform linux-x86_64", linux_release)
        self.assertIn("--platform darwin-arm64", macos_release)
        for release_budget in (linux_release, macos_release):
            self.assertIn("--require-measured", release_budget)
            self.assertIn("scripts/build-release.sh", release_budget)
        self.assertIn("ROTTWEILER_SELF_HOSTED_RUNNERS", runner_contract)
        self.assertIn("exit 1", runner_contract)
        self.assertEqual(workflow.count("runner-contract"), 4)
        soak = workflow_job(workflow, "eight-hour-soak")
        self.assertIn(
            "needs: [runner-contract, linux-performance-build, macos-performance-build]",
            soak,
        )
        self.assertNotIn("cargo-release.sh build", soak)
        self.assertIn("${{ matrix.artifact }}", soak)
        self.assertIn("${{ matrix.tui_artifact }}", soak)
        self.assertIn("rottweiler-soak-binary.noindex/rw", soak)
        self.assertIn("packages/tui/dist/rottweiler-tui", soak)
        self.assertNotIn("bun run build", soak)
        self.assertNotIn("--require-measured", soak)
        wsl2 = workflow_job(workflow, "wsl2-acceptance")
        self.assertIn("runs-on: windows-2025", wsl2)
        self.assertNotIn("self-hosted", wsl2)
        self.assertNotIn("needs: runner-contract", wsl2)
        self.assertIn("wsl.exe --install --distribution Ubuntu-24.04 --no-launch", wsl2)
        self.assertIn("wsl.exe --distribution Ubuntu-24.04 --exec", wsl2)
        self.assertIn("scripts/provision-wsl-ci.sh", wsl2)
        hosted_terminal_bench = workflow_job(workflow, "terminal-bench")
        self.assertIn("runs-on: ubuntu-24.04", hosted_terminal_bench)
        self.assertNotIn("self-hosted", hosted_terminal_bench)
        self.assertNotIn("needs: runner-contract", hosted_terminal_bench)
        terminal_bench = workflow.split("  terminal-bench:", 1)[1]
        job_environment = terminal_bench.split("    steps:", 1)[0]
        run_step = terminal_bench.split("      - name: Run pinned 20-task subset", 1)[1].split(
            "      - name:", 1
        )[0]
        self.assertNotIn("secrets.ROTTWEILER_EVAL_API_KEY", job_environment)
        self.assertIn("secrets.OPENAI_API_KEY", run_step)
        self.assertIn("secrets.ANTHROPIC_API_KEY", run_step)
        self.assertIn('ROTTWEILER_EVAL_API_KEY="$key"', run_step)
        self.assertIn("if: steps.release_version.outputs.major != '0'", run_step)
        self.assertNotIn("models: read", workflow)

    def test_release_soak_requires_reviewed_measured_baselines(self) -> None:
        workflow = (ROOT / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        soak = workflow_job(workflow, "release-soak")

        self.assertIn("--suite soak", soak)
        self.assertIn("--require-measured", soak)
        self.assertIn("if: needs.runner-contract.outputs.release_major != '0'", soak)
        self.assertIn("needs: [runner-contract, build-linux, build-macos]", soak)
        qualification = workflow_job(workflow, "qualification-gate")
        self.assertIn("if: ${{ always() }}", qualification)
        self.assertIn("release-soak", qualification)
        self.assertIn("test \"$RELEASE_SOAK_RESULT\" = skipped", qualification)
        self.assertIn("test \"$RELEASE_SOAK_RESULT\" = success", qualification)
        self.assertIn("not_claimed_for_pre_v1", qualification)
        sign_and_publish = workflow_job(workflow, "sign-and-publish")
        self.assertIn("qualification-gate", sign_and_publish)
        self.assertNotIn("release-soak", sign_and_publish.split("    runs-on:", 1)[0])
        self.assertIn("Casks/rottweiler.rb", sign_and_publish)
        self.assertIn(
            'packaging/homebrew/README.md "$tap/README.md"', sign_and_publish
        )
        tap_readme = (ROOT / "packaging/homebrew/README.md").read_text(encoding="utf-8")
        self.assertIn(
            "brew install --cask Boredphilosopher96/tap/rottweiler", tap_readme
        )
        self.assertNotIn("--HEAD", tap_readme)
        contract = workflow_job(workflow, "runner-contract")
        self.assertIn("actions: read", contract)
        self.assertIn("release-preflight.yml/runs", contract)
        self.assertIn("release-candidate.py verify", contract)
        self.assertIn('head_sha="$GITHUB_SHA"', contract)
        self.assertIn('"head_branch": "main"', contract)
        self.assertNotIn("  linux-performance-build:", workflow)

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
        self.assertIn("--release-version", preflight)
        self.assertIn("python3 scripts/check-dogfood-gate.py", preflight)
        self.assertIn("id: repository_readiness", preflight)
        self.assertIn("id: protected_configuration", preflight)
        self.assertIn("id: dogfood", preflight)
        self.assertEqual(preflight.count("continue-on-error: true"), 3)
        self.assertIn("Enforce aggregate release readiness", preflight)
        self.assertIn("steps.repository_readiness.outcome", preflight)
        self.assertIn("steps.protected_configuration.outcome", preflight)
        self.assertIn("steps.dogfood.outcome", preflight)
        self.assertIn("id: release_version", preflight)
        self.assertIn("RELEASE_MAJOR: ${{ steps.release_version.outputs.major }}", preflight)
        self.assertIn('if [ "$RELEASE_MAJOR" -ge 1 ]; then', preflight)
        self.assertIn("if: steps.release_version.outputs.major != '0'", preflight)
        self.assertIn("uses: ./.github/workflows/performance.yml", preflight)
        self.assertIn("needs: repository-prerequisites", preflight)
        candidate = workflow_job(preflight, "seal-candidate")
        self.assertIn("needs: [repository-prerequisites, protected-performance]", candidate)
        self.assertIn("release-candidate.py create", candidate)
        self.assertIn("manual-performance-linux-x86_64", candidate)
        self.assertIn("manual-performance-darwin-arm64", candidate)
        self.assertNotIn("gh release create", preflight)
        self.assertNotIn("git push", preflight)
        self.assertNotIn("HOMEBREW_TAP_TOKEN:", preflight.split("    steps:", 1)[0])
        self.assertIn("HOMEBREW_TAP_DEPLOY_KEY", preflight)
        self.assertNotIn("HOMEBREW_TAP_TOKEN", preflight)

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
        self.assertIn("--re \"${{ matrix.filter }}\"", workflow)
        self.assertIn("scripts/check-mutation-score.py", workflow)
        self.assertIn("--minimum-score \"${{ matrix.minimum_score }}\"", workflow)
        self.assertIn("--component llvm-tools-preview,rustfmt,clippy", workflow)
        self.assertIn("--component rustfmt,clippy", workflow)
        self.assertNotIn("--in-place", workflow)
        self.assertEqual(
            workflow.count("oven-sh/setup-bun@"),
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

    def test_dependabot_is_weekly_grouped_and_does_not_hide_major_upgrades(
        self,
    ) -> None:
        configuration = (ROOT / ".github/dependabot.yml").read_text(encoding="utf-8")
        self.assertEqual(configuration.count("package-ecosystem:"), 5)
        self.assertNotIn("interval: daily", configuration)
        self.assertEqual(configuration.count("interval: weekly"), 2)
        self.assertEqual(
            configuration.count("multi-ecosystem-group: application-dependencies"),
            4,
        )
        self.assertEqual(
            configuration.count("multi-ecosystem-group: automation-dependencies"),
            1,
        )
        self.assertEqual(configuration.count("open-pull-requests-limit: 1"), 5)
        self.assertNotIn("version-update:semver-major", configuration)

    def test_javascript_dependencies_do_not_use_version_specific_patches(
        self,
    ) -> None:
        for package_name in ("tui", "plugin-sdk", "plugin-docs"):
            package_root = ROOT / "packages" / package_name
            manifest = (package_root / "package.json").read_text(encoding="utf-8")
            self.assertNotIn("patchedDependencies", manifest)
            lockfile = package_root / "bun.lock"
            if lockfile.exists():
                self.assertNotIn(
                    "patchedDependencies", lockfile.read_text(encoding="utf-8")
                )
            patches = package_root / "patches"
            self.assertEqual(list(patches.glob("*.patch")), [])

    def test_signed_release_is_serialized_and_downloads_only_unsigned_archives(
        self,
    ) -> None:
        workflow = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        sign_and_publish = workflow.split("  sign-and-publish:", 1)[1]
        self.assertIn(
            "concurrency:\n  group: signed-release\n  cancel-in-progress: false",
            workflow,
        )
        self.assertEqual(workflow.count("actions/attest@"), 3)
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
        dogfood_step = release_gate.split(
            "      - name: Validate protected fourteen-day dogfood evidence", 1
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
        self.assertIn("if: ${{ !startsWith(github.ref_name, 'v0.') }}", paid_step)
        self.assertIn("if: ${{ !startsWith(github.ref_name, 'v0.') }}", dogfood_step)
        self.assertNotIn(
            "secrets.ROTTWEILER_EVAL_API_KEY", terminal_job_environment
        )
        for secret in ("secrets.OPENAI_API_KEY", "secrets.ANTHROPIC_API_KEY"):
            self.assertIn(secret, eval_step)
        self.assertIn('ROTTWEILER_EVAL_API_KEY="$key"', eval_step)
        self.assertIn("if: steps.release_version.outputs.major != '0'", eval_step)
        self.assertNotIn("models: read", terminal_bench)
        self.assertIn("runs-on: ubuntu-24.04", terminal_bench)
        self.assertNotIn("self-hosted", terminal_bench)
        wsl2 = workflow_job(workflow, "wsl2-acceptance")
        self.assertIn("runs-on: windows-2025", wsl2)
        self.assertNotIn("self-hosted", wsl2)
        self.assertIn("wsl.exe --install --distribution Ubuntu-24.04 --no-launch", wsl2)
        self.assertIn("scripts/provision-wsl-ci.sh", wsl2)
        deployment = workflow_job(workflow, "deploy-update-repository")
        self.assertIn("needs: sign-and-publish", deployment)
        self.assertIn("ref: gh-pages", deployment)
        self.assertIn("git -C site push origin HEAD:gh-pages", deployment)
        self.assertNotIn("rm -rf", deployment)

    def test_preflight_uses_job_scoped_eval_authentication_contract(self) -> None:
        workflow = (ROOT / ".github/workflows/release-preflight.yml").read_text(
            encoding="utf-8"
        )
        self.assertNotIn("ROTTWEILER_EVAL_API_KEY", workflow)
        self.assertNotIn("EVAL_API_KEY", workflow)
        self.assertIn(
            "EVAL_MODEL must select an immutable dated OpenAI or Anthropic model",
            workflow,
        )
        required = workflow.split("required=(", 1)[1].split(")", 1)[0]
        v1_required = workflow.split('if [ "$RELEASE_MAJOR" -ge 1 ]; then', 1)[1].split(
            "fi", 1
        )[0]
        self.assertNotIn("EVAL_MODEL", required)
        self.assertNotIn("TERMINAL_BENCH_BASELINE_JSON", required)
        self.assertIn("EVAL_MODEL", v1_required)
        self.assertIn("TERMINAL_BENCH_BASELINE_JSON", v1_required)

    def test_rerun_artifacts_preserve_producers_and_version_evidence(self) -> None:
        workflows = {
            name: (ROOT / f".github/workflows/{name}.yml").read_text(encoding="utf-8")
            for name in ("performance", "nightly", "release", "release-preflight")
        }

        for workflow_name in ("performance", "nightly"):
            workflow = workflows[workflow_name]
            with self.subTest(workflow=workflow_name, producer="linux"):
                producer = workflow_job(workflow, "linux-performance-build")
                self.assertIn(
                    "name: linux-performance-rw-${{ github.run_id }}", producer
                )
                self.assertNotIn("github.run_attempt", producer)
                self.assertIn("overwrite: true", producer)

        macos_producer = workflow_job(workflows["nightly"], "macos-performance-build")
        self.assertIn(
            "name: macos-performance-rw-${{ github.run_id }}", macos_producer
        )
        self.assertNotIn("github.run_attempt", macos_producer)
        self.assertIn("overwrite: true", macos_producer)

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
                "release-gate-evidence-${{ github.ref_name }}-${{ github.run_attempt }}",
                "release-terminal-bench-${{ github.ref_name }}-${{ github.run_attempt }}",
                "release-soak-${{ matrix.platform }}-${{ github.ref_name }}-${{ github.run_attempt }}",
                "release-qualification-${{ github.ref_name }}-${{ github.run_attempt }}",
            ),
            "release-preflight": (
                "release-preflight-${{ github.run_id }}-${{ github.run_attempt }}",
                "release-candidate-${{ github.run_id }}-${{ github.run_attempt }}",
            ),
        }
        for workflow_name, evidence_names in expected_evidence_names.items():
            for artifact_name in evidence_names:
                with self.subTest(workflow=workflow_name, evidence=artifact_name):
                    self.assertIn(f"name: {artifact_name}", workflows[workflow_name])


if __name__ == "__main__":
    unittest.main()
