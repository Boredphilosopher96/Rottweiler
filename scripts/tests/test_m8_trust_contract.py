from __future__ import annotations

import importlib.util
from pathlib import Path
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[2]
GATE = REPO / "crates/rw-cli/tests/m8_release_gate.py"
SPEC = importlib.util.spec_from_file_location("m8_release_gate", GATE)
assert SPEC is not None
assert SPEC.loader is not None
M8 = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(M8)


class M8TrustContractTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.workspace = Path(temporary.name).resolve() / "workspace"
        self.inventory = self.workspace / ".agents" / "mcp.toml"
        self.inventory.parent.mkdir(parents=True)
        self.inventory.write_text(
            "[servers.alpha]\nenabled = true\n", encoding="utf-8"
        )
        self.digest = "a" * 64

    def assessment(
        self,
        state: str,
        *,
        change_lines: list[str] | None = None,
        prompt: bool = False,
    ) -> bytes:
        lines = [
            f"workspace: {self.workspace}",
            f"state: {state}",
            "project extension inventory:",
            "  .agents/mcp.toml [mcp] "
            f"{self.inventory.stat().st_size} bytes hash {self.digest}",
        ]
        lines.extend(change_lines or [])
        output = "\n".join(lines) + "\n"
        if prompt:
            output += M8.PROJECT_TRUST_PROMPT
        return output.encode()

    def test_accepts_exact_initial_addition_and_persisted_inventory(self) -> None:
        initial = self.assessment(
            "Untrusted",
            change_lines=[
                "changes since last trust:",
                "  + .agents/mcp.toml",
            ],
            prompt=True,
        )
        approved_hash = M8.validate_project_trust_inventory(
            initial,
            self.workspace,
            self.inventory,
            expected_state="Untrusted",
            require_initial_addition=True,
            require_prompt=True,
        )

        persisted = self.assessment("Trusted")
        self.assertEqual(
            M8.validate_project_trust_inventory(
                persisted,
                self.workspace,
                self.inventory,
                expected_state="Trusted",
                expected_hash=approved_hash,
            ),
            self.digest,
        )

    def test_rejects_initial_prompt_without_exact_addition(self) -> None:
        for change_lines in (
            [],
            ["changes since last trust:", "  + .agents/hooks.toml"],
        ):
            with self.subTest(change_lines=change_lines):
                with self.assertRaisesRegex(RuntimeError, "exact inventory addition"):
                    M8.validate_project_trust_inventory(
                        self.assessment(
                            "Untrusted", change_lines=change_lines, prompt=True
                        ),
                        self.workspace,
                        self.inventory,
                        expected_state="Untrusted",
                        require_initial_addition=True,
                        require_prompt=True,
                    )

    def test_rejects_changes_after_trust_is_persisted(self) -> None:
        persisted_with_changes = self.assessment(
            "Trusted",
            change_lines=[
                "changes since last trust:",
                "  + .agents/mcp.toml",
            ],
        )

        with self.assertRaisesRegex(RuntimeError, "unexpectedly reported"):
            M8.validate_project_trust_inventory(
                persisted_with_changes,
                self.workspace,
                self.inventory,
                expected_state="Trusted",
                expected_hash=self.digest,
            )


if __name__ == "__main__":
    unittest.main()
