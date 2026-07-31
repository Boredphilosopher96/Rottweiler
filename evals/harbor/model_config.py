"""Pure configuration builder for the Harbor adapter."""

from __future__ import annotations

import json
import re


MODEL = re.compile(
    r"([a-z0-9][a-z0-9._-]*)/([A-Za-z0-9][A-Za-z0-9._:/-]*)(?:@([0-9]{4}-[0-9]{2}-[0-9]{2}))?"
)
PROVIDER_KINDS = {
    "anthropic": "anthropic",
    "github": "openai",
    "openai": "openai",
}
PROVIDER_CREDENTIAL_ENVIRONMENTS = {
    "anthropic": "ANTHROPIC_API_KEY",
    "github": "GITHUB_MODELS_TOKEN",
    "openai": "OPENAI_API_KEY",
}


def build_model_config(model_name: str | None) -> str:
    """Bind a pinned benchmark model to an explicitly configured API provider."""
    match = MODEL.fullmatch(model_name or "")
    if match is None:
        raise ValueError("model must be a pinned provider/model identifier")
    provider = match.group(1)
    kind = PROVIDER_KINDS.get(provider)
    if kind is None:
        raise ValueError("live eval provider must be one of: anthropic, github, openai")
    model = match.group(2)
    version = match.group(3)
    if provider == "github" and version is None:
        raise ValueError("GitHub Models evals must pin the catalog version")
    configured_model = f"{provider}/{model}"
    config = (
        "[models]\n"
        'default = "benchmark"\n'
        "[models.aliases]\n"
        f"benchmark = [{json.dumps(configured_model)}]\n"
        f"[providers.{provider}]\n"
        f"kind = {json.dumps(kind)}\n"
    )
    if provider == "github":
        config += (
            'base_url = "https://models.github.ai/inference/chat/completions"\n'
            'api_key_env = "GITHUB_MODELS_TOKEN"\n'
        )
    return config


def credential_environment(model_name: str | None) -> str:
    """Return the one provider credential name required by the selected model."""
    match = MODEL.fullmatch(model_name or "")
    if match is None or match.group(1) not in PROVIDER_CREDENTIAL_ENVIRONMENTS:
        raise ValueError("live eval model does not select a supported credential")
    return PROVIDER_CREDENTIAL_ENVIRONMENTS[match.group(1)]
