#!/usr/bin/env python3
"""Pin the metadata fallback from an already downloaded models.dev response."""
import argparse
import datetime
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FIELDS = ("name", "reasoning", "reasoning_options", "tool_call", "modalities", "limit", "cost")
PROVIDERS = ("openai", "anthropic", "github-copilot", "google", "openrouter")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("--date", required=True, type=datetime.date.fromisoformat)
    args = parser.parse_args()
    raw = args.source.read_bytes()
    if len(raw) > 64 * 1024 * 1024:
        raise ValueError("catalog exceeds 64 MiB")
    source = json.loads(raw)
    catalog = {}
    for provider in PROVIDERS:
        models = {}
        for name, model in source[provider]["models"].items():
            cost = model.get("cost") or {}
            if cost.get("input") is None or cost.get("output") is None:
                continue
            models[name] = {field: model[field] for field in FIELDS if field in model}
        catalog[provider] = {"models": models}
    snapshot = {
        "source_url": "https://models.dev/api.json",
        "snapshot_date": args.date.isoformat(),
        "revision": "sha256:" + hashlib.sha256(raw).hexdigest(),
        "catalog": catalog,
    }
    target = ROOT / "crates/rw-providers/data/models-dev.json"
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(json.dumps(snapshot, separators=(",", ":"), sort_keys=True) + "\n")
    print(f"{target}: {sum(len(p['models']) for p in catalog.values())} models")


if __name__ == "__main__":
    main()
