import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "release-candidate.py"
SOURCE_SHA = "2f3e71187396ff6a3cda7c0b11c225027f8416c4"


class ReleaseCandidateTests(unittest.TestCase):
    def evidence(self, root: Path) -> tuple[Path, Path, Path]:
        readiness = root / "readiness"
        linux = root / "linux"
        darwin = root / "darwin"
        for path, name in (
            (readiness, "release-readiness.json"),
            (linux, "headless.json"),
            (darwin, "headless.json"),
        ):
            path.mkdir()
            (path / name).write_text(f"{path.name} evidence\n", encoding="utf-8")
        return readiness, linux, darwin

    def command(
        self,
        action: str,
        path: Path,
        readiness: Path,
        linux: Path,
        darwin: Path,
        *,
        version: str = "0.1.1",
    ) -> list[str]:
        return [
            sys.executable,
            str(SCRIPT),
            action,
            "--path",
            str(path),
            "--repository",
            "Boredphilosopher96/Rottweiler",
            "--source-sha",
            SOURCE_SHA,
            "--version",
            version,
            "--run-id",
            "32374072929",
            "--run-attempt",
            "1",
            "--readiness",
            str(readiness),
            "--linux-evidence",
            str(linux),
            "--darwin-evidence",
            str(darwin),
        ]

    def test_create_and_verify_exact_pre_v1_candidate(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "release-candidate.json"
            evidence = self.evidence(root)
            subprocess.run(self.command("create", path, *evidence), check=True)
            document = json.loads(path.read_text(encoding="utf-8"))
            verified = subprocess.run(
                self.command("verify", path, *evidence),
                capture_output=True,
                text=True,
                check=False,
            )

        self.assertEqual(verified.returncode, 0, verified.stdout + verified.stderr)
        self.assertEqual(document["tag"], "v0.1.1")
        self.assertEqual(document["qualification"], "pre-v1")
        self.assertEqual(document["source_sha"], SOURCE_SHA)
        self.assertEqual(document["evidence"]["protected_performance"], "passed")
        self.assertRegex(document["artifacts"]["linux_performance"]["sha256"], r"^[0-9a-f]{64}$")

    def test_verify_rejects_tampering_and_wrong_expected_version(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "release-candidate.json"
            evidence = self.evidence(root)
            subprocess.run(self.command("create", path, *evidence), check=True)
            document = json.loads(path.read_text(encoding="utf-8"))
            document["source_sha"] = "0" * 40
            path.write_text(json.dumps(document), encoding="utf-8")
            tampered = subprocess.run(
                self.command("verify", path, *evidence),
                capture_output=True,
                text=True,
                check=False,
            )
            wrong_version = subprocess.run(
                self.command("verify", path, *evidence, version="0.1.2"),
                capture_output=True,
                text=True,
                check=False,
            )

        self.assertNotEqual(tampered.returncode, 0)
        self.assertIn("does not match", tampered.stderr)
        self.assertNotEqual(wrong_version.returncode, 0)

    def test_v1_candidate_cannot_claim_pre_v1_qualification(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "release-candidate.json"
            evidence = self.evidence(root)
            subprocess.run(
                self.command("create", path, *evidence, version="1.0.0"), check=True
            )
            document = json.loads(path.read_text(encoding="utf-8"))

        self.assertEqual(document["qualification"], "v1")


if __name__ == "__main__":
    unittest.main()
