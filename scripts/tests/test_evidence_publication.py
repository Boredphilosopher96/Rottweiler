"""Evidence retries must neither rerun gates nor hide missing publication."""

import os
from pathlib import Path
import subprocess
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
ACTION = ROOT / ".github/actions/retain-evidence/action.yml"


class EvidencePublicationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.steps = yaml.safe_load(ACTION.read_text())["runs"]["steps"]

    def test_only_publication_retries_and_preserves_uncertain_first_upload(self) -> None:
        uploads = [step for step in self.steps if "uses" in step]
        self.assertEqual(len(uploads), 2)
        self.assertEqual(uploads[0]["uses"], uploads[1]["uses"])
        self.assertTrue(uploads[0]["uses"].startswith("actions/upload-artifact@"))
        self.assertEqual(uploads[0]["with"]["name"], "${{ inputs.name }}")
        self.assertEqual(uploads[1]["with"]["name"], "${{ inputs.name }}-publication-retry")
        for upload in uploads:
            self.assertNotIn("overwrite", upload["with"])
            self.assertEqual(upload["with"]["if-no-files-found"], "error")
            self.assertEqual(upload["with"]["path"], "${{ inputs.path }}")

    def test_cancellation_prevents_uploads_and_success_never_retries(self) -> None:
        for step in self.steps:
            self.assertIn("!cancelled()", step["if"])
        for step in self.steps[1:3]:
            self.assertIn("steps.first.outcome == 'failure'", step["if"])
        self.assertNotIn("continue-on-error", self.steps[-1])

    def test_terminal_shell_requires_at_least_one_success(self) -> None:
        for first in ("success", "failure", "skipped", "cancelled", ""):
            for retry in ("success", "failure", "skipped", "cancelled", ""):
                with self.subTest(first=first, retry=retry):
                    result = subprocess.run(
                        ["bash", "-e", "-c", self.steps[-1]["run"]],
                        env=dict(os.environ, FIRST_OUTCOME=first, RETRY_OUTCOME=retry),
                        capture_output=True, timeout=5,
                    )
                    self.assertEqual(result.returncode == 0, "success" in (first, retry))

    def test_ci_evidence_has_one_owner_and_candidate_producers_are_explicit(self) -> None:
        workflow = yaml.safe_load((ROOT / ".github/workflows/ci.yml").read_text())
        count = 0
        for job in workflow["jobs"].values():
            for step in job.get("steps", []):
                if step.get("with", {}).get("path") == "ci-results/":
                    count += 1
                    self.assertEqual(step["uses"], "./.github/actions/retain-evidence")
                    self.assertEqual(step["if"], "always()")
                    self.assertNotIn("continue-on-error", step)
                if step.get("name") == "Upload verified native candidate":
                    self.assertTrue(step["uses"].startswith("actions/upload-artifact@"))
                    self.assertNotIn("continue-on-error", step)
        self.assertGreater(count, 10)


if __name__ == "__main__":
    unittest.main()
