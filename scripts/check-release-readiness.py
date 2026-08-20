#!/usr/bin/env python3
"""Fail-closed validation of repository-owned release prerequisites."""

from __future__ import annotations

import argparse
import base64
import binascii
import json
from pathlib import Path
import re
import tomllib
from typing import Any


EXPECTED_PLATFORMS = ("darwin-arm64", "linux-x86_64")
EXPECTED_SUITES = ("core", "soak")
VERSION_PATTERN = re.compile(
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
    r"(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?"
)
EXPECTED_UPDATE_FILES = (
    "root-chain.json",
    "stable.spec.json",
    "beta.spec.json",
)


def load_json(path: Path, blockers: list[str]) -> dict[str, Any] | None:
    if not path.is_file():
        blockers.append(f"missing required release input: {path.as_posix()}")
        return None
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        blockers.append(f"invalid JSON in {path.as_posix()}: {error}")
        return None
    if not isinstance(value, dict):
        blockers.append(f"{path.as_posix()} must contain a JSON object")
        return None
    return value


def validate_baseline(path: Path, release_major: int, blockers: list[str]) -> None:
    baseline = load_json(path, blockers)
    if baseline is None:
        return
    platforms = baseline.get("platforms")
    if not isinstance(platforms, dict):
        blockers.append(f"{path.as_posix()} has no platform map")
        return
    for platform in EXPECTED_PLATFORMS:
        platform_value = platforms.get(platform)
        suites = platform_value.get("suites") if isinstance(platform_value, dict) else None
        if not isinstance(suites, dict):
            blockers.append(f"{platform} has no performance suites")
            continue
        required_suites = EXPECTED_SUITES if release_major >= 1 else ("core",)
        for suite in required_suites:
            suite_value = suites.get(suite)
            if not isinstance(suite_value, dict):
                blockers.append(f"{platform}/{suite} baseline is missing")
                continue
            if suite_value.get("baseline_kind") != "measured":
                blockers.append(
                    f"{platform}/{suite} baseline is not measured on a protected runner"
                )
            provenance = suite_value.get("provenance")
            if not isinstance(provenance, str) or len(provenance.strip()) < 12:
                blockers.append(f"{platform}/{suite} baseline has no reviewed provenance")
            metrics = suite_value.get("metrics")
            if not isinstance(metrics, dict) or not metrics:
                blockers.append(f"{platform}/{suite} baseline has no metrics")


def validate_root_chain(path: Path, blockers: list[str]) -> None:
    document = load_json(path, blockers)
    if document is None:
        return
    roots = document.get("roots")
    if not isinstance(roots, list) or not roots:
        blockers.append(f"{path.as_posix()} must contain a non-empty roots array")
        return
    for index, root in enumerate(roots):
        if not isinstance(root, dict) or set(root) != {"version", "envelope"}:
            blockers.append(f"{path.as_posix()} root {index} is not a signed envelope")
            continue
        version = root.get("version")
        if not isinstance(version, int) or isinstance(version, bool) or version < 1:
            blockers.append(f"{path.as_posix()} root {index} has an invalid version")
        encoded_envelope = root.get("envelope")
        if not isinstance(encoded_envelope, str) or not encoded_envelope:
            blockers.append(f"{path.as_posix()} root {index} has no envelope bytes")
            continue
        try:
            envelope_bytes = base64.b64decode(encoded_envelope, validate=True)
            if base64.b64encode(envelope_bytes).decode("ascii") != encoded_envelope:
                raise ValueError("non-canonical envelope base64")
            envelope = json.loads(envelope_bytes)
            if not isinstance(envelope, dict) or set(envelope) != {"payload", "signatures"}:
                raise ValueError("invalid signed envelope shape")
            encoded_payload = envelope.get("payload")
            signatures = envelope.get("signatures")
            if not isinstance(encoded_payload, str) or not isinstance(signatures, list):
                raise ValueError("invalid signed envelope fields")
            payload_bytes = base64.b64decode(encoded_payload, validate=True)
            if base64.b64encode(payload_bytes).decode("ascii") != encoded_payload:
                raise ValueError("non-canonical payload base64")
            payload = json.loads(payload_bytes)
            if not isinstance(payload, dict) or payload.get("role") != "root":
                raise ValueError("invalid root payload")
            if payload.get("version") != version:
                raise ValueError("entry version does not match signed root payload")
        except (
            binascii.Error,
            json.JSONDecodeError,
            UnicodeDecodeError,
            ValueError,
        ) as error:
            blockers.append(f"{path.as_posix()} root {index} is malformed: {error}")


def validate_channel_specs(
    update_root: Path, release_version: str, blockers: list[str]
) -> None:
    versions: dict[str, int] = {}
    for channel in ("stable", "beta"):
        path = update_root / f"{channel}.spec.json"
        document = load_json(path, blockers)
        if document is None:
            continue
        if document.get("schema_version") != 1 or document.get("role") != "release":
            blockers.append(f"{path.as_posix()} has an invalid schema or role")
        if document.get("channel") != channel:
            blockers.append(f"{path.as_posix()} has the wrong channel")
        version = document.get("version")
        if not isinstance(version, int) or isinstance(version, bool) or version < 1:
            blockers.append(f"{path.as_posix()} has an invalid metadata version")
        else:
            versions[channel] = version
        targets = document.get("targets")
        if not isinstance(targets, dict):
            blockers.append(f"{path.as_posix()} has no targets")
            continue
        if set(targets) != set(EXPECTED_PLATFORMS):
            blockers.append(
                f"{path.as_posix()} must target exactly {', '.join(EXPECTED_PLATFORMS)}"
            )
        for platform, target in targets.items():
            if not isinstance(target, dict) or target.get("version") != release_version:
                blockers.append(
                    f"{path.as_posix()} target {platform} does not match release version {release_version}"
                )
    if len(versions) == 2 and versions["stable"] != versions["beta"]:
        blockers.append("stable and beta specs must share one metadata version")


def inspect(repository: Path, release_version: str) -> dict[str, Any]:
    blockers: list[str] = []
    if VERSION_PATTERN.fullmatch(release_version) is None:
        blockers.append("release version must be canonical semantic version without a leading v")
        release_major = 0
    else:
        release_major = int(release_version.split(".", 1)[0])
    baseline = repository / "benchmarks" / "performance-baseline.json"
    update_root = repository / "release" / "update"
    validate_baseline(baseline, release_major, blockers)
    validate_root_chain(update_root / EXPECTED_UPDATE_FILES[0], blockers)
    validate_channel_specs(update_root, release_version, blockers)
    qualification = "pre-v1" if release_major == 0 else "v1"
    return {
        "schema_version": 1,
        "status": "ready" if not blockers else "blocked",
        "release_version": release_version,
        "release_major": release_major,
        "qualification": qualification,
        "evidence": {
            "core_baselines": "required",
            "protected_soak": (
                "not_claimed_for_pre_v1" if release_major == 0 else "required"
            ),
        },
        "blockers": sorted(set(blockers)),
    }


def workspace_version(repository: Path) -> str:
    manifest = repository / "Cargo.toml"
    try:
        document = tomllib.loads(manifest.read_text(encoding="utf-8"))
        version = document["workspace"]["package"]["version"]
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError, KeyError, TypeError) as error:
        raise ValueError(f"could not resolve workspace release version: {error}") from error
    if not isinstance(version, str):
        raise ValueError("workspace release version must be a string")
    return version


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repository", type=Path, default=Path.cwd())
    parser.add_argument("--release-version")
    parser.add_argument("--output", type=Path)
    arguments = parser.parse_args()

    repository = arguments.repository.resolve()
    release_version = arguments.release_version or workspace_version(repository)
    result = inspect(repository, release_version)
    encoded = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if arguments.output is not None:
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        arguments.output.write_text(encoded, encoding="utf-8")
    print(encoded, end="")
    return 0 if result["status"] == "ready" else 1


if __name__ == "__main__":
    raise SystemExit(main())
