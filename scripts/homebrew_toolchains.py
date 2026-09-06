#!/usr/bin/env python3
"""Resolve verified HEAD build resources from the source toolchain/platform owners."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import platform
import re
import tomllib
import urllib.request

from native_candidate import pinned_toolchains
from release_contract import load_contract

ROOT = Path(__file__).resolve().parents[1]
DIGESTS = "contracts/toolchain-artifacts.json"


def bun_resources(root: Path) -> dict[str, str]:
    _, version = pinned_toolchains(root)
    contract = load_contract(root / "contracts/release-contract.json")
    result = {}
    for target in contract.platforms:
        arch = "aarch64" if target.rust_arch == "aarch64" else "x64-baseline"
        asset = f"bun-{target.system.lower()}-{arch}.zip"
        result[target.id] = f"https://github.com/oven-sh/bun/releases/download/bun-v{version}/{asset}"
    return result


def verified_resources(root: Path) -> dict[str, dict[str, str]]:
    urls = bun_resources(root)
    document = json.loads((root / DIGESTS).read_text())
    if set(document) != {"schema_version", "sha256"} or document["schema_version"] != 1:
        raise ValueError("unsupported toolchain artifact inventory")
    digests = document["sha256"]
    if not isinstance(digests, dict) or set(digests) != set(urls.values()):
        raise ValueError("toolchain artifact identities differ from source pins; run scripts/homebrew_toolchains.py --refresh")
    if any(not isinstance(value, str) or re.fullmatch(r"[a-f0-9]{64}", value) is None for value in digests.values()):
        raise ValueError("toolchain artifact SHA-256 must be lowercase hexadecimal")
    return {target: {"url": url, "sha256": digests[url]} for target, url in urls.items()}


def manifest(root: Path, system: str, machine: str) -> dict:
    contract = load_contract(root / "contracts/release-contract.json")
    target = contract.resolve_platform(system, machine)
    rust, _ = pinned_toolchains(root)
    configuration = tomllib.loads((root / "rust-toolchain.toml").read_text())["toolchain"]
    return {"rust": rust, "profile": configuration["profile"], "components": configuration["components"],
            "bun": verified_resources(root)[target.id]}


def refresh(root: Path) -> None:
    urls = bun_resources(root)
    checksum_url = next(iter(urls.values())).rsplit("/", 1)[0] + "/SHASUMS256.txt"
    # Only the immutable official release selected by the source owner is fetched.
    with urllib.request.urlopen(checksum_url, timeout=30) as response:
        source = response.read(256 * 1024 + 1)
    if len(source) > 256 * 1024:
        raise ValueError("official checksum inventory is too large")
    checksums = {}
    for line in source.decode("utf-8").splitlines():
        fields = line.split()
        if len(fields) == 2 and re.fullmatch(r"[a-f0-9]{64}", fields[0]):
            if fields[1] in checksums:
                raise ValueError("duplicate official artifact checksum")
            checksums[fields[1]] = fields[0]
    pinned = {url: checksums[url.rsplit("/", 1)[1]] for url in urls.values()}
    (root / DIGESTS).write_text(json.dumps({"schema_version": 1, "sha256": pinned}, indent=2) + "\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--refresh", action="store_true", help="pin official release checksums for the source versions")
    parser.add_argument("--check", action="store_true", help="check every supported platform without downloading")
    args = parser.parse_args()
    if args.refresh:
        refresh(ROOT)
    elif args.check:
        verified_resources(ROOT)
    else:
        print(json.dumps(manifest(ROOT, platform.system(), platform.machine())))


if __name__ == "__main__":
    main()
