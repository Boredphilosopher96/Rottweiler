import base64
import json
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/check-release-channel-advance.py"


def write_spec(path: Path, channel: str, version: int) -> None:
    path.write_text(
        json.dumps(
            {
                "schema_version": 1,
                "role": "release",
                "version": version,
                "channel": channel,
            }
        )
    )


def write_envelope(path: Path, channel: str, version: int) -> None:
    payload = json.dumps(
        {
            "schema_version": 1,
            "role": "release",
            "version": version,
            "channel": channel,
        },
        separators=(",", ":"),
    ).encode()
    path.write_text(
        json.dumps(
            {
                "payload": base64.b64encode(payload).decode(),
                "signatures": [{"key_id": "release", "signature": "fixture"}],
            }
        )
    )


class ReleaseChannelAdvanceTests(unittest.TestCase):
    def run_check(
        self, candidate: int, prior: int | None
    ) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stable_spec = root / "stable.spec.json"
            beta_spec = root / "beta.spec.json"
            output = root / "evidence.json"
            write_spec(stable_spec, "stable", candidate)
            write_spec(beta_spec, "beta", candidate)
            command = [
                "python3",
                str(SCRIPT),
                "--stable-spec",
                str(stable_spec),
                "--beta-spec",
                str(beta_spec),
                "--output",
                str(output),
            ]
            if prior is not None:
                stable = root / "stable.json"
                beta = root / "beta.json"
                write_envelope(stable, "stable", prior)
                write_envelope(beta, "beta", prior)
                command.extend(
                    [
                        "--previous-stable",
                        str(stable),
                        "--previous-beta",
                        str(beta),
                    ]
                )
            result = subprocess.run(command, text=True, capture_output=True)
            if result.returncode == 0:
                result.evidence = json.loads(output.read_text())  # type: ignore[attr-defined]
            return result

    def test_accepts_first_publication_at_version_one(self) -> None:
        result = self.run_check(1, None)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.evidence["candidate_metadata_version"], 1)  # type: ignore[attr-defined]
        self.assertIsNone(result.evidence["prior_metadata_version"])  # type: ignore[attr-defined]

    def test_accepts_exact_successor(self) -> None:
        result = self.run_check(2, 1)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.evidence["prior_metadata_version"], 1)  # type: ignore[attr-defined]

    def test_rejects_reused_or_skipped_versions(self) -> None:
        for candidate in (1, 3):
            with self.subTest(candidate=candidate):
                result = self.run_check(candidate, 1)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("advance exactly from 1 to 2", result.stderr)

    def test_rejects_noninitial_version_without_prior_metadata(self) -> None:
        result = self.run_check(2, None)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("first channel publication", result.stderr)


if __name__ == "__main__":
    unittest.main()
