import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


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
            "  macos-performance-build:", 1
        )[0]
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
        release = workflow.split("  cross-platform-release:", 1)[1].split(
            "  eight-hour-soak:", 1
        )[0]
        runner_contract = workflow.split("  runner-contract:", 1)[1].split(
            "  fuzz:", 1
        )[0]
        self.assertNotIn("continue-on-error", release)
        self.assertIn("ROTTWEILER_SELF_HOSTED_RUNNERS", runner_contract)
        self.assertIn("exit 1", runner_contract)
        self.assertEqual(workflow.count("needs: runner-contract"), 3)
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


if __name__ == "__main__":
    unittest.main()
