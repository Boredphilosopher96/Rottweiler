from __future__ import annotations

import unittest

from evals.harbor.model_config import build_model_config, credential_environment


class HarborModelConfigTests(unittest.TestCase):
    def test_openai_and_anthropic_configure_the_provider(self) -> None:
        openai = build_model_config("openai/gpt-5-mini")
        self.assertIn("benchmark = [\"openai/gpt-5-mini\"]", openai)
        self.assertIn("[providers.openai]\nkind = \"openai\"", openai)

        anthropic = build_model_config("anthropic/claude-sonnet-pinned")
        self.assertIn("[providers.anthropic]\nkind = \"anthropic\"", anthropic)
        self.assertEqual(credential_environment("openai/gpt-5-mini"), "OPENAI_API_KEY")
        self.assertEqual(
            credential_environment("anthropic/claude-sonnet-pinned"), "ANTHROPIC_API_KEY"
        )

    def test_unknown_or_unpinned_provider_is_rejected(self) -> None:
        for value in (
            None,
            "",
            "gpt-5-mini",
            "gateway/model",
            "openai/$MODEL",
            "github/openai/gpt-4.1",
            "github/openai/gpt-4.1@2025-04-14",
            "openai/gpt-4.1@2025-04-14",
        ):
            with self.subTest(value=value), self.assertRaises(ValueError):
                build_model_config(value)


if __name__ == "__main__":
    unittest.main()
