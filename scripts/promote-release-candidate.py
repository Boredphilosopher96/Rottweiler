#!/usr/bin/env python3
"""Promote one exact, prebuilt native candidate into a tag release."""
from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import tomllib

import native_candidate
from release_contract import load_contract


REPO = Path(__file__).resolve().parents[1]
UPDATE_ENVIRONMENT = (
    "ROTTWEILER_UPDATE_ROOT_VERSION",
    "ROTTWEILER_UPDATE_ROOT_THRESHOLD",
    "ROTTWEILER_UPDATE_ROOT_KEYS_JSON",
    "ROTTWEILER_UPDATE_BASE_URL",
)


def required_environment(environment: dict[str, str]) -> dict[str, str]:
    values = {}
    for name in UPDATE_ENVIRONMENT:
        value = environment.get(name)
        if not value:
            raise ValueError(f"release promotion requires {name}")
        values[name] = value
    if not values["ROTTWEILER_UPDATE_BASE_URL"].endswith("/"):
        raise ValueError("ROTTWEILER_UPDATE_BASE_URL must end with /")
    return values


def latest_root_role(repo: Path) -> dict:
    try:
        document = json.loads((repo / "release/update/root-chain.json").read_text())
        latest = document["roots"][-1]
        envelope = json.loads(base64.b64decode(latest["envelope"], validate=True))
        root = json.loads(base64.b64decode(envelope["payload"], validate=True))
        if latest["version"] != root["version"]:
            raise ValueError("latest signed root version differs from its chain entry")
        return root
    except (OSError, KeyError, IndexError, TypeError, json.JSONDecodeError, ValueError) as error:
        raise ValueError("could not read the latest signed updater root") from error


def validate_update_configuration(repo: Path, values: dict[str, str]) -> None:
    root = latest_root_role(repo)
    try:
        embedded = json.loads(values["ROTTWEILER_UPDATE_ROOT_KEYS_JSON"])
        expected = {key_id: root["keys"][key_id] for key_id in root["root_key_ids"]}
        root_version = int(values["ROTTWEILER_UPDATE_ROOT_VERSION"])
        root_threshold = int(values["ROTTWEILER_UPDATE_ROOT_THRESHOLD"])
    except (KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        raise ValueError("configured updater root values are invalid") from error
    if root_version != root.get("version"):
        raise ValueError("configured updater root version does not match the latest signed root")
    if root_threshold != root.get("root_threshold"):
        raise ValueError("configured updater root threshold does not match the latest signed root")
    if embedded != expected:
        raise ValueError("configured updater root keys do not match the latest signed root role")
    for encoded in embedded.values():
        try:
            decoded = base64.b64decode(encoded, validate=True)
        except (TypeError, ValueError) as error:
            raise ValueError("configured updater root key is not canonical base64") from error
        if len(decoded) != 32 or base64.b64encode(decoded).decode("ascii") != encoded:
            raise ValueError("configured updater root key is not canonical 32-byte base64")


def workspace_version(repo: Path) -> str:
    try:
        version = tomllib.loads((repo / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    except (OSError, KeyError, TypeError, tomllib.TOMLDecodeError) as error:
        raise ValueError("could not resolve the workspace release version") from error
    if not isinstance(version, str) or not version:
        raise ValueError("workspace release version must be a nonempty string")
    return version


def promote(candidate: Path, version: str, platform: str, github_output: Path, *,
            repo: Path = REPO, environment: dict[str, str] | None = None) -> Path:
    environment = dict(os.environ if environment is None else environment)
    release_sha = environment.get("GITHUB_SHA")
    if not release_sha:
        raise ValueError("release promotion requires GITHUB_SHA")
    if version != workspace_version(repo):
        raise ValueError("requested release version does not match the workspace")
    if environment.get("GITHUB_REF") != f"refs/tags/v{version}":
        raise ValueError("release promotion must run from the exact version tag")
    load_contract(repo / "contracts/release-contract.json").platform(platform)
    update_values = required_environment(environment)
    validate_update_configuration(repo, update_values)

    receipt = native_candidate.verify(candidate, repo)
    identity = receipt["identity"]
    if identity["source"]["commit"] != release_sha:
        raise ValueError("candidate source commit differs from GITHUB_SHA")
    if identity["version"] != version:
        raise ValueError("candidate version differs from the requested release")
    if identity["platform"] != platform:
        raise ValueError("candidate platform differs from the requested release")
    recorded = identity["profile"].get("environment")
    if not isinstance(recorded, dict):
        raise ValueError("candidate build environment is invalid")
    for name, value in update_values.items():
        expected = hashlib.sha256(value.encode()).hexdigest()
        if recorded.get(name) != expected:
            raise ValueError(f"candidate build environment differs for {name}")

    archive = candidate.absolute() / receipt["components"]["archive"]["path"]
    if any(character in str(archive) for character in "\r\n"):
        raise ValueError("candidate archive path cannot contain a line break")
    with github_output.open("a", encoding="utf-8") as output:
        output.write(f"path={archive}\n")
    return archive


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--github-output", type=Path, required=True)
    args = parser.parse_args()
    print(promote(args.candidate, args.version, args.platform, args.github_output))


if __name__ == "__main__":
    main()
