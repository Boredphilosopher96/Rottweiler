#!/usr/bin/env python3
"""Fail closed when a 2026-08-22 field-audit remediation contract regresses."""

from __future__ import annotations

import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def text(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SystemExit(f"field-audit remediation check failed: {message}")


def require_contains(path: str, *needles: str) -> None:
    contents = text(path)
    for needle in needles:
        require(needle in contents, f"{path} is missing {needle!r}")


def main() -> int:
    m4 = text("crates/rw-cli/tests/m4_release_gate.py")
    baseline = json.loads(text("benchmarks/performance-baseline.json"))
    baseline_text = json.dumps(baseline, sort_keys=True)
    require("tui_first_paint_p99_us" not in m4, "obsolete first-paint metric remains in M4")
    require("tui_first_paint_p99_us" not in baseline_text, "obsolete first-paint metric remains in baseline")
    require(
        baseline_text.count("installed_first_interactive_max_us") == 2
        and baseline_text.count("installed_first_version_max_us") == 2,
        "installed first-launch metrics are missing from a platform baseline",
    )
    require_contains(
        "crates/rw-cli/tests/m4_release_gate.py",
        "tui_process_start_p99_us",
        "tui_interactive_p99_us",
        "installed_first_launch_gate",
        "installed_first_version_max_us",
        "installed_first_interactive_max_us",
        "REPRESENTATIVE_PRICING_MODEL_COUNT = 4_000",
        "supervisor_parent_death_gate",
    )
    require_contains(
        "packages/tui/test/perf/performance.test.ts",
        "tui_tool_output_frame_p95_us",
        "tool_output_delta",
        "new TreeSitterClient",
    )
    require_contains("crates/rw-tools/src/bash.rs", "copy_stream_preserves_utf8_split_across_reads", "from_utf8(&pending)")
    require_contains("packages/tui/src/transport/sse.ts", "indexOf(0x0a", "subarray")
    require_contains("packages/tui/src/recycle-state.ts", "schemaVersion", "scrollTop", "draft")
    require_contains("packages/tui/scripts/opentui-rss-harness.ts", "createTestRenderer")
    require_contains("packages/tui/src/tree-sitter-runtime.ts", "TREE_SITTER_ASSET_DIGEST", '"tree-sitter"')
    require_contains("packages/tui/src/tree-sitter-client.ts", "registerTreeSitterParsersLazily")
    require_contains(
        "packages/tui/src/render/format.ts",
        "formatStatusContext",
        "limit unknown",
        "formatStatusModel",
        "formatStatusSessionCost",
        'return "quota —"',
    )
    require_contains(
        "crates/rw-store/src/session.rs",
        "garbage_collect_empty_sessions",
        "turn_count",
        "backfill_unknown_turn_counts",
    )
    require_contains(
        "crates/rw-cli/src/main.rs",
        "UPDATED (UTC)",
        '"turn_count":session.turn_count',
    )
    require_contains("crates/rw-cli/src/parent_death.rs", "NOTE_EXIT", "set_parent_process_death_signal")
    require_contains(
        "benchmarks/release-optimization-2026-08-22.json",
        '"samples": 100',
        '"3"',
        '"z"',
    )
    require_contains("scripts/cargo-release.sh", "*-apple-darwin) optimization=3")

    for path in (
        "packaging/homebrew/rottweiler.cask.rb.in",
        "packaging/homebrew/rottweiler.rb.in",
        "packaging/homebrew/rottweiler-head.rb",
    ):
        contents = text(path)
        require("ROTTWEILER_PACKAGE_MANAGER" not in contents, f"{path} still injects a wrapper marker")
        require("symlink" in contents or "binary " in contents, f"{path} does not expose a symlink")

    ci = text(".github/workflows/ci.yml")
    release = text(".github/workflows/release.yml")
    require("file:{os.environ['SDK']}" not in ci, "CI still rewrites the SDK dependency to source")
    require("npm pack" in ci and "npm install --prefix" in ci, "CI does not consume the packed SDK")
    require("npm publish --access public" in release, "tag workflow does not publish the SDK")
    require("npm view \"@rottweiler/plugin@$version\"" in release, "release lacks registry consumer proof")

    package = json.loads(text("packages/plugin-sdk/package.json"))
    cargo = text("Cargo.toml")
    require(f'version = "{package["version"]}"' in cargo, "SDK and product versions differ")
    scaffold = text("packages/plugin-sdk/src/scaffold.ts")
    require(scaffold.count('path: "manifest.json"') == 1, "scaffold manifest source is not singular")
    require("parsePluginManifest(manifestDocument)" in scaffold, "scaffold does not import inert manifest data")
    require_contains("crates/rw-cli/src/plugin_dev.rs", '"host":"rottweiler"')
    require_contains(
        "docs/design/typescript-source-plugin-host.md",
        "one sandboxed host process for each active TypeScript plugin",
        "two-pass operation",
        "SessionExtensionSnapshot",
    )
    require_contains("docs/03-DECISIONS.md", "ADR-027")
    print("field-audit remediation contract: pass")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
