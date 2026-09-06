#!/usr/bin/env python3
"""Enforce the internal Rust dependency directions from 02-ARCHITECTURE."""

import json
from pathlib import Path
import re
import subprocess
import sys
import tomllib


ALLOWED = {
    "rw-operation-contract": set(),
    "rw-resources": set(),
    "rw-memory-derive": set(),
    "rw-macos-bootstrap": set(),
    "rw-plugin-protocol": {"rw-operation-contract", "rw-types"},
    "rw-types": {"rw-operation-contract", "rw-memory-derive"},
    "rw-store": {"rw-resources", "rw-types"},
    "rw-providers": {"rw-resources", "rw-types"},
    "rw-context": {"rw-providers", "rw-types"},
    "rw-sandbox": {"rw-resources", "rw-types", "rw-macos-bootstrap"},
    "rw-intel": {"rw-types"},
    "rw-tools": {"rw-resources", "rw-operation-contract", "rw-intel", "rw-sandbox", "rw-types"},
    "rw-mcp": {"rw-tools", "rw-types"},
    "rw-ext": {"rw-resources", "rw-operation-contract", "rw-plugin-protocol", "rw-providers", "rw-tools", "rw-types"},
    "rw-core": {
        "rw-resources",
        "rw-context",
        "rw-ext",
        "rw-mcp",
        "rw-plugin-protocol",
        "rw-providers",
        "rw-store",
        "rw-tools",
        "rw-types",
    },
    # Owns concrete provider/tool/storage/extension assembly while keeping the
    # engine in rw-core independent from executable frontends.
    "rw-runtime": {
        "rw-resources",
        "rw-core",
        "rw-ext",
        "rw-mcp",
        "rw-plugin-protocol",
        "rw-providers",
        "rw-store",
        "rw-tools",
        "rw-types",
    },
    # Private process boundary for the heavyweight WASM runtime. The public
    # `rw` binary talks to it through rw-ext's bounded wire protocol and does
    # not link Wasmtime itself.
    # Protocol integration tests construct explicitly approved executable artifacts
    # through the same tool/sandbox ownership API as the runtime caller.
    "rw-wasm-host": {"rw-ext", "rw-plugin-protocol", "rw-tools", "rw-types"},
    "rw-cli": {
        "rw-core",
        "rw-ext",
        "rw-mcp",
        "rw-plugin-protocol",
        "rw-providers",
        "rw-runtime",
        "rw-store",
        "rw-tools",
        "rw-types",
    },
    # Codegen imports each contract from its implementation owner.
    "xtask": {"rw-types", "rw-operation-contract", "rw-plugin-protocol", "rw-providers", "rw-store", "rw-tools"},
}

RUNTIME_COMPOSITION_FILES = {
    "extension_config.rs",
    "extension_runtime.rs",
    "history.rs",
    "mode_recovery.rs",
    "plugin_process.rs",
    "project_commands.rs",
    "session_host.rs",
    "session_runtime.rs",
    "subagent_metadata.rs",
    "workflow_runtime.rs",
}


def validate_source_layout(repo_root: Path) -> list[str]:
    """Reject architectural laundering that Cargo metadata cannot detect."""
    failures = []
    core_source = repo_root / "crates" / "rw-core" / "src"
    cli_source = repo_root / "crates" / "rw-cli" / "src"
    runtime_source = repo_root / "crates" / "rw-runtime" / "src"

    for source_root in (core_source, cli_source):
        for path in source_root.rglob("*.rs"):
            if "runtime_support" in path.read_text():
                failures.append(f"runtime facade laundering is forbidden: {path.relative_to(repo_root)}")

    runtime_lib = runtime_source / "lib.rs"
    if runtime_lib.is_file():
        contents = runtime_lib.read_text()
        if re.search(r"pub\s+use\s+rw_(?:core|ext|mcp|providers|store|tools|types)::", contents):
            failures.append("rw-runtime must not re-export lower-layer crate APIs")
    else:
        failures.append("rw-runtime/src/lib.rs is missing")

    runtime_manifest = repo_root / "crates" / "rw-runtime" / "Cargo.toml"
    if runtime_manifest.is_file():
        dependencies = tomllib.loads(runtime_manifest.read_text()).get("dependencies", {})
        terminal_dependencies = {"rustyline", "crossterm", "ratatui", "clap"} & dependencies.keys()
        if terminal_dependencies:
            failures.append(f"rw-runtime must not depend on terminal clients: {sorted(terminal_dependencies)}")
    for path in runtime_source.rglob("*.rs"):
        if "tests" in path.parts or path.name == "tests.rs":
            continue
        if re.search(r"\b(?:e?print(?:ln)?!|(?:std::)?io::std(?:in|out|err)\s*\()", path.read_text()):
            failures.append(f"terminal I/O belongs to clients: {path.relative_to(repo_root)}")

    missing_runtime_files = sorted(
        name for name in RUNTIME_COMPOSITION_FILES if not (runtime_source / name).is_file()
    )
    if missing_runtime_files:
        failures.append(f"rw-runtime composition files missing: {missing_runtime_files}")

    duplicated_cli_files = sorted(
        name for name in RUNTIME_COMPOSITION_FILES if (cli_source / name).exists()
    )
    if duplicated_cli_files:
        failures.append(f"rw-cli must not own runtime composition files: {duplicated_cli_files}")

    cli_manifest = (repo_root / "crates" / "rw-cli" / "Cargo.toml").read_text()
    if "rw-runtime.workspace = true" not in cli_manifest:
        failures.append("rw-cli must consume the shared rw-runtime crate")
    return failures

FORBIDDEN_JS_RUNTIMES = {
    "boa_engine",
    "deno_core",
    "quick-js",
    "rquickjs",
    "rusty_v8",
}


def main() -> int:
    repo_root = Path(__file__).resolve().parent.parent
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
    failures = validate_source_layout(repo_root)
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
