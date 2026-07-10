#!/usr/bin/env python3
"""Enforce the internal Rust dependency directions from 02-ARCHITECTURE."""

import json
import subprocess
import sys


ALLOWED = {
    "rw-types": set(),
    "rw-store": {"rw-types"},
    "rw-providers": {"rw-types"},
    "rw-context": {"rw-providers", "rw-types"},
    "rw-sandbox": {"rw-types"},
    "rw-intel": {"rw-types"},
    "rw-tools": {"rw-intel", "rw-sandbox", "rw-types"},
    "rw-mcp": {"rw-tools", "rw-types"},
    "rw-ext": {"rw-providers", "rw-tools", "rw-types"},
    "rw-core": {
        "rw-context",
        "rw-ext",
        "rw-mcp",
        "rw-providers",
        "rw-store",
        "rw-tools",
        "rw-types",
    },
    "rw-cli": {"rw-core", "rw-store"},
    "xtask": {"rw-types"},
}

FORBIDDEN_JS_RUNTIMES = {
    "boa_engine",
    "deno_core",
    "quick-js",
    "rquickjs",
    "rusty_v8",
}


def main() -> int:
    metadata = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--locked", "--format-version", "1"],
            text=True,
        )
    )
    workspace_package_names = {
        package["name"]
        for package in metadata["packages"]
        if package["id"] in metadata["workspace_members"]
    }
    expected_package_names = set(ALLOWED)
    failures = []
    if workspace_package_names != expected_package_names:
        missing = sorted(expected_package_names - workspace_package_names)
        unknown = sorted(workspace_package_names - expected_package_names)
        if missing:
            failures.append(f"architecture crates missing from workspace: {missing}")
        if unknown:
            failures.append(f"workspace crates missing from direction policy: {unknown}")

    package_names = {package["name"] for package in metadata["packages"]}
    embedded_js_runtimes = sorted(package_names & FORBIDDEN_JS_RUNTIMES)
    if embedded_js_runtimes:
        failures.append(
            f"Rust engine graph embeds JavaScript runtimes: {embedded_js_runtimes}"
        )

    for package in metadata["packages"]:
        name = package["name"]
        if name not in workspace_package_names or name not in ALLOWED:
            continue
        internal = {
            dependency["name"]
            for dependency in package["dependencies"]
            if dependency["name"] in workspace_package_names
        }
        unexpected = internal - ALLOWED[name]
        if unexpected:
            failures.append(f"{name} must not depend on {sorted(unexpected)}")

    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1

    print("internal dependency directions: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
