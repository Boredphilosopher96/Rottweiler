import json
import subprocess
import tempfile
import unittest
from pathlib import Path


REPOSITORY = Path(__file__).resolve().parents[2]
PUBLISHER = REPOSITORY / "scripts/publish-docs-site.py"


class DocsPublisherTest(unittest.TestCase):
    def test_overlay_replaces_docs_and_preserves_updates(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            checkout = root / "site"
            build = root / "build"
            (checkout / "updates").mkdir(parents=True)
            build.mkdir()
            (checkout / "updates/stable.json").write_text('{"signed":true}\n')
            (checkout / "old/index.html").parent.mkdir()
            (checkout / "old/index.html").write_text("old\n")
            (checkout / ".rottweiler-docs-manifest.json").write_text(
                json.dumps({"schema_version": 1, "files": ["old/index.html"]}) + "\n"
            )
            (build / "docs").mkdir()
            (build / "docs/index.html").write_text("new\n")
            (build / "docs-index.json").write_text("{}\n")
            (build / "index.html").write_text("home\n")
            files = ["docs-index.json", "docs/index.html", "index.html"]
            (build / ".rottweiler-docs-manifest.json").write_text(
                json.dumps({"schema_version": 1, "files": files}) + "\n"
            )

            subprocess.run(["git", "init", "-b", "gh-pages", checkout], check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["git", "-C", checkout, "add", "."], check=True)
            subprocess.run(
                ["git", "-C", checkout, "-c", "user.name=test", "-c", "user.email=test@example.com", "commit", "-m", "baseline"],
                check=True,
                stdout=subprocess.DEVNULL,
            )

            command = ["python3", str(PUBLISHER), "--site", str(build), "--checkout", str(checkout)]
            subprocess.run(command, check=True)
            self.assertEqual((checkout / "updates/stable.json").read_text(), '{"signed":true}\n')
            self.assertFalse((checkout / "old").exists())
            self.assertEqual((checkout / "index.html").read_text(), "home\n")
            self.assertEqual((checkout / "docs/index.html").read_text(), "new\n")
            self.assertEqual(
                subprocess.run(
                    ["git", "-C", checkout, "status", "--short", "--", "updates"],
                    check=True,
                    text=True,
                    stdout=subprocess.PIPE,
                ).stdout,
                "",
            )

            first_status = subprocess.run(
                ["git", "-C", checkout, "status", "--short"], check=True, text=True, stdout=subprocess.PIPE
            ).stdout
            subprocess.run(command, check=True)
            second_status = subprocess.run(
                ["git", "-C", checkout, "status", "--short"], check=True, text=True, stdout=subprocess.PIPE
            ).stdout
            self.assertEqual(second_status, first_status)


if __name__ == "__main__":
    unittest.main()
