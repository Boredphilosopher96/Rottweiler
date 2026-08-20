#!/usr/bin/env python3
"""Fail before tagging when release-channel metadata does not advance exactly."""

from __future__ import annotations

import argparse
import base64
import json
from pathlib import Path


MAX_METADATA_BYTES = 1024 * 1024


def read_json(path: Path) -> object:
    payload = path.read_bytes()
    if not payload or len(payload) > MAX_METADATA_BYTES:
        raise ValueError(f"{path} is empty or oversized")
    return json.loads(payload)


def spec_version(path: Path, expected_channel: str) -> int:
    document = read_json(path)
    if not isinstance(document, dict):
        raise ValueError(f"{path} must contain a JSON object")
    version = document.get("version")
    if (
        document.get("schema_version") != 1
        or document.get("role") != "release"
        or document.get("channel") != expected_channel
        or not isinstance(version, int)
        or isinstance(version, bool)
        or version < 1
    ):
        raise ValueError(f"{path} has an invalid release-channel identity")
    return version


def prior_version(path: Path, expected_channel: str) -> int:
    envelope = read_json(path)
    if not isinstance(envelope, dict) or set(envelope) != {"payload", "signatures"}:
        raise ValueError(f"{path} has an invalid signed-envelope shape")
    encoded = envelope.get("payload")
    signatures = envelope.get("signatures")
    if not isinstance(encoded, str) or not isinstance(signatures, list) or not signatures:
        raise ValueError(f"{path} has an invalid signed envelope")
    try:
        payload = base64.b64decode(encoded, validate=True)
    except Exception as error:
        raise ValueError(f"{path} payload is not canonical base64") from error
    if (
        not payload
        or len(payload) > MAX_METADATA_BYTES
        or base64.b64encode(payload).decode("ascii") != encoded
    ):
        raise ValueError(f"{path} payload is empty, oversized, or non-canonical")
    document = json.loads(payload)
    if not isinstance(document, dict):
        raise ValueError(f"{path} payload must contain a JSON object")
    version = document.get("version")
    if (
        document.get("schema_version") != 1
        or document.get("role") != "release"
        or document.get("channel") != expected_channel
        or not isinstance(version, int)
        or isinstance(version, bool)
        or version < 1
    ):
        raise ValueError(f"{path} payload has an invalid release-channel identity")
    return version


def check(args: argparse.Namespace) -> dict[str, object]:
    stable = spec_version(args.stable_spec, "stable")
    beta = spec_version(args.beta_spec, "beta")
    if beta != stable:
        raise ValueError("stable and beta specs must use one metadata version")

    previous = (args.previous_stable, args.previous_beta)
    if (previous[0] is None) != (previous[1] is None):
        raise ValueError("previous stable and beta metadata must be supplied together")
    if previous[0] is None:
        if stable != 1:
            raise ValueError("the first channel publication must use metadata version 1")
        prior = None
    else:
        prior_stable = prior_version(previous[0], "stable")
        prior_beta = prior_version(previous[1], "beta")
        if prior_beta != prior_stable:
            raise ValueError("previous stable and beta metadata versions disagree")
        expected = prior_stable + 1
        if stable != expected:
            raise ValueError(
                f"new channel metadata version must advance exactly from {prior_stable} to {expected}"
            )
        prior = prior_stable

    return {
        "schema_version": 1,
        "status": "ready",
        "prior_metadata_version": prior,
        "candidate_metadata_version": stable,
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--stable-spec", type=Path, required=True)
    parser.add_argument("--beta-spec", type=Path, required=True)
    parser.add_argument("--previous-stable", type=Path)
    parser.add_argument("--previous-beta", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        evidence = check(args)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        raise SystemExit(str(error)) from error
    args.output.write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
