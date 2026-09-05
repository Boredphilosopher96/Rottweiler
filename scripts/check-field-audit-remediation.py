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
        "supervisor_parent_death_gate",
    )
    require_contains("scripts/m4_gate_support.py", "REPRESENTATIVE_PRICING_MODEL_COUNT = 4_000")
    require_contains(
        "packages/tui/test/perf/performance.test.ts",
        "tui_tool_output_frame_p95_us",
        "tool_output_delta",
        "new TreeSitterClient",
    )
    require_contains("crates/rw-tools/src/bash/tests/output.rs", "copy_stream_preserves_utf8_split_across_reads")
    require_contains("crates/rw-tools/src/bash/output.rs", "from_utf8(&pending)")
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
        "crates/rw-store/src/session/event_log.rs",
        "garbage_collect_empty_sessions",
    )
    require_contains("crates/rw-store/src/session/index.rs", "turn_count", "transaction.commit()")
    require_contains(
        "crates/rw-store/src/session/sqlite_schema.rs",
        "validate_sessions",
        "validate_accounting",
        "UnsupportedSqliteSchema",
        "turn_count INTEGER NOT NULL DEFAULT 0",
    )
    require_contains(
        "crates/rw-store/src/session/tests/index.rs",
        "opening_an_unsupported_index_rejects_without_backfill_or_mutation",
        "read_only_listing_rejects_a_pre_turn_count_index",
    )
    require_contains(
        "crates/rw-store/src/session/sqlite_schema_tests.rs",
        "derived_rebuild_preserves_authority_and_rolls_back_on_accounting_conflict",
        "explicit_search_rebuild_can_replace_an_unsupported_derived_schema",
    )
    store_sources = "\n".join(
        path.read_text(encoding="utf-8")
        for path in (ROOT / "crates/rw-store/src/session").rglob("*.rs")
    )
    for removed in ("backfill_unknown_turn_counts", "ensure_accounting_columns", "remove_legacy_turn_uniqueness", "session_index_has_turn_count"):
        require(f"fn {removed}(" not in store_sources, f"removed compatibility path {removed} returned")
    require_contains(
        "crates/rw-cli/src/main.rs",
        "UPDATED (UTC)",
        '"turn_count":session.turn_count',
    )
    require_contains("crates/rw-cli/src/parent_death.rs", "NOTE_EXIT", "set_parent_process_death_signal")
    require_contains(
        "crates/rw-context/src/budget.rs",
        ".min(self.context_window_tokens / 2)",
        "default_reserve_cannot_exhaust_the_context_window",
    )
    require_contains(
        "crates/rw-core/src/engine/turn/mod.rs",
        "resolved_overflow_policy",
        ".validate()",
        "recent_failures: VecDeque<Option<String>>",
        "window_capacity: threshold.saturating_mul(4)",
    )
    require_contains(
        "crates/rw-providers/src/anthropic.rs",
        "mark_last_cacheable_message_block",
        "last_stable_system",
        'Some("text" | "image" | "tool_use" | "tool_result")',
    )
    require_contains(
        "crates/rw-sandbox/src/lib.rs",
        "const SENSITIVE_HOME_SUFFIXES",
        "fn sensitive_home_roots",
    )
    require_contains("crates/rw-sandbox/src/linux.rs", "for lexical in sensitive_home_roots(home)")
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
    scaffold_files = text("packages/plugin-sdk/fixtures/scaffold/files.txt")
    scaffold_source = text("packages/plugin-sdk/fixtures/scaffold/src/index.ts")
    require(
        scaffold_files.count("manifest.json\tmanifest.json") == 1,
        "scaffold manifest source is not singular",
    )
    require("files.txt" in scaffold, "SDK renderer does not consume the canonical scaffold mapping")
    require(
        "parsePluginManifest(manifestDocument)" in scaffold_source,
        "scaffold does not import inert manifest data",
    )
    require_contains(
        "crates/rw-runtime/src/source_plugin.rs",
        "SourcePluginResolver",
        "let discovered = self.graph",
        "let rebuilt = self.bundle",
        "if rebuilt != discovered",
        "publish_bundle",
        "parse_bun_lock",
    )
    require_contains(
        "packages/plugin-host/src/index.ts",
        "SOURCE_HOST_ABI",
        "SOURCE_BUNDLE_FORMAT",
        "dynamic-import",
        "rejectSymlinkComponents",
        "await runPlugin(loaded.plugin)",
    )
    require_contains(
        "crates/rw-core/src/engine/session_extension.rs",
        "SessionExtensionSnapshot",
        "SessionExtensionController",
    )
    require_contains(
        "crates/rw-runtime/src/extension_runtime/development.rs",
        "RuntimeSessionExtensionController",
        "candidate.shutdown().await",
        "state.active.replace(candidate)",
    )
    require_contains(
        "crates/rw-cli/src/plugin_dev.rs",
        'CAPABILITY_HEADER, "plugin_development"',
        "AttachDevelopmentPlugin",
        "DetachDevelopmentPlugin",
        "retaining last good generation",
    )
    require_contains(
        "contracts/release-contract.json",
        '"id": "plugin_host"',
        '"path": "bin/rottweiler-plugin-host"',
    )
    require(
        "--compile" not in text("packages/plugin-sdk/fixtures/scaffold/package.json"),
        "the TypeScript scaffold still embeds a Bun runtime",
    )
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
