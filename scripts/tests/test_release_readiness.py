import base64
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "check-release-readiness.py"
PLATFORMS = ("darwin-arm64", "linux-x86_64")


def measured_baseline() -> dict[str, object]:
    return {
        "schema_version": 1,
        "maximum_regression_fraction": 0.1,
        "platforms": {
            platform: {
                "suites": {
                    suite: {
                        "baseline_kind": "measured",
                        "provenance": f"protected runner measurement for {platform}",
                        "metrics": {"metric": 1},
                    }
                    for suite in ("core", "soak")
                }
            }
            for platform in PLATFORMS
        },
    }


def channel_spec(channel: str, release_version: str = "1.0.0") -> dict[str, object]:
    return {
        "schema_version": 1,
        "role": "release",
        "version": 1,
        "expires_unix": 2_000_000_000,
        "channel": channel,
        "release_notes": "preflight fixture",
        "targets": {
            platform: {
                "version": release_version,
                "url": f"https://updates.example.test/rottweiler-{release_version}-{platform}.tar.gz",
            }
            for platform in PLATFORMS
        },
    }


class ReleaseReadinessTests(unittest.TestCase):
    def fixture(self, root: Path, *, release_version: str = "1.0.0") -> None:
        (root / "benchmarks").mkdir()
        update = root / "release" / "update"
        update.mkdir(parents=True)
        (root / "benchmarks" / "performance-baseline.json").write_text(
            json.dumps(measured_baseline()), encoding="utf-8"
        )
        payload = base64.b64encode(
            json.dumps(
                {
                    "schema_version": 1,
                    "role": "root",
                    "version": 1,
                }
            ).encode()
        ).decode("ascii")
        (update / "root-chain.json").write_text(
            json.dumps(
                {
                    "roots": [
                        {
                            "version": 1,
                            "envelope": base64.b64encode(
                                json.dumps(
                                    {"payload": payload, "signatures": []}
                                ).encode()
                            ).decode("ascii")
                        }
                    ]
                }
            ),
            encoding="utf-8",
        )
        for channel in ("stable", "beta"):
            (update / f"{channel}.spec.json").write_text(
                json.dumps(channel_spec(channel, release_version)), encoding="utf-8"
            )

    def run_check(
        self, root: Path, *, release_version: str = "1.0.0"
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--repository",
                str(root),
                "--release-version",
                release_version,
            ],
            check=False,
            capture_output=True,
            text=True,
        )

    def test_accepts_complete_measured_public_inputs(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(json.loads(result.stdout)["status"], "ready")

    def test_rejects_bootstrap_baseline(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            path = root / "benchmarks" / "performance-baseline.json"
            baseline = json.loads(path.read_text(encoding="utf-8"))
            baseline["platforms"]["darwin-arm64"]["suites"]["core"][
                "baseline_kind"
            ] = "bootstrap"
            path.write_text(json.dumps(baseline), encoding="utf-8")
            result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("darwin-arm64/core baseline is not measured", result.stdout)

    def test_pre_v1_requires_core_but_records_soak_as_not_claimed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root, release_version="0.1.0")
            path = root / "benchmarks" / "performance-baseline.json"
            baseline = json.loads(path.read_text(encoding="utf-8"))
            for platform in PLATFORMS:
                baseline["platforms"][platform]["suites"]["soak"][
                    "baseline_kind"
                ] = "bootstrap"
            path.write_text(json.dumps(baseline), encoding="utf-8")
            result = self.run_check(root, release_version="0.1.0")
            document = json.loads(result.stdout)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(document["qualification"], "pre-v1")
        self.assertEqual(
            document["evidence"]["protected_soak"], "not_claimed_for_pre_v1"
        )

    def test_v1_requires_measured_soak_baseline(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            path = root / "benchmarks" / "performance-baseline.json"
            baseline = json.loads(path.read_text(encoding="utf-8"))
            baseline["platforms"]["linux-x86_64"]["suites"]["soak"][
                "baseline_kind"
            ] = "bootstrap"
            path.write_text(json.dumps(baseline), encoding="utf-8")
            result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("linux-x86_64/soak baseline is not measured", result.stdout)

    def test_rejects_missing_signing_inputs_and_split_channel_versions(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            update = root / "release" / "update"
            (update / "root-chain.json").unlink()
            beta = json.loads((update / "beta.spec.json").read_text(encoding="utf-8"))
            beta["version"] = 2
            (update / "beta.spec.json").write_text(json.dumps(beta), encoding="utf-8")
            result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing required release input", result.stdout)
        self.assertIn("stable and beta specs must share one metadata version", result.stdout)


if __name__ == "__main__":
    unittest.main()
