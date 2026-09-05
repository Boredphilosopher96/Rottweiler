import { describe, expect, test } from "bun:test"

import { CustomSpeedScroll, diffStats, filetypeForPath, formatCost, formatSessionCost, formatStatusContext, formatStatusModel, formatStatusSessionCost, formatTokenCount, getScrollAcceleration, minimalUnifiedDiff, presentableUnifiedDiff, splitDiffVisualRows, toolOutputText, truncateUnifiedDiff, turnMarkdown, turnReasoningMarkdown } from "../src/render"
import { embeddedParserConfigurations } from "../src/tree-sitter-runtime"

describe("bounded retained rendering", () => {
  test("keeps unknown context, route billing, and model identity truthful", () => {
    expect(formatStatusContext({
      turn_id: "1",
      stable_prefix_hash: "fixture",
      used_tokens: "3900",
      usable_tokens: "0",
      reserved_tokens: "0",
      context_window_known: false,
      context_window_reason: "provider did not report a context limit",
      cache_breakpoints: [],
      items: [],
    })).toBe("ctx 3.9k · limit unknown")

    const models = [{
      id: "openai_codex/gpt-5.4-mini",
      displayName: "GPT-5.4 mini",
      provider: "openai_codex",
      aliases: ["fast"],
      current: true,
      available: true,
      status: null,
      vision: true,
      thinking: true,
      toolCalling: true,
    }]
    expect(formatStatusModel("fast", "openai_codex", models))
      .toBe("openai_codex/gpt-5.4-mini")

    const zeroCost = {
      utc_day: "2026-08-22",
      subscription_quota: null,
      session_usage: {
        input_tokens: "0",
        output_tokens: "0",
        cache_read_tokens: "0",
        cache_write_tokens: "0",
        reasoning_tokens: "0",
      },
      session_cost_micros_usd: "0",
      session_ai_credit_micros: "0",
      session_subscription_tokens: "0",
      daily_cost_micros_usd: "0",
      daily_ai_credit_micros: "0",
      daily_subscription_tokens: "0",
      trailing_minute_cost_micros_usd: "0",
      trailing_minute_ai_credit_micros: "0",
      trailing_minute_subscription_tokens: "0",
      cache_hit_basis_points: 0,
      session_cost_cap_micros_usd: null,
      daily_cost_cap_micros_usd: null,
      session_ai_credit_cap_micros: null,
      daily_ai_credit_cap_micros: null,
      session_token_cap: null,
      daily_token_cap: null,
      spend_rate_alarm_micros_usd_per_minute: null,
      ai_credit_rate_alarm_micros_per_minute: null,
      token_rate_alarm_per_minute: null,
      hard_cap_reached: false,
      session_monetary_accounting_complete: true,
      daily_monetary_accounting_complete: true,
      session_subscription_quota_entries: "0",
      session_cost_unavailable_entries: "0",
      session_non_usd_monetary_entries: "0",
      daily_subscription_quota_entries: "0",
      daily_cost_unavailable_entries: "0",
      daily_non_usd_monetary_entries: "0",
    }
    expect(formatStatusSessionCost(zeroCost, "openai_codex", "3900"))
      .toBe("quota —")
    expect(formatStatusSessionCost(zeroCost, "github_copilot", "3900"))
      .toBe("credits —")
    const quota = { ...zeroCost, session_monetary_accounting_complete: false,
      session_subscription_quota_entries: "2", subscription_quota: { used: "9007199254740993.000001", unit: "requests" } }
    expect(formatSessionCost(quota)).toBe("9007199254740993.000001 requests")
    expect(formatSessionCost({ ...quota, subscription_quota: null })).toBe("0 tokens")
  })

  test("only exposes filetypes backed by the embedded parser catalog", () => {
    const configured = new Set(
      embeddedParserConfigurations("/tmp/parser-assets")
        .flatMap((parser) => [parser.filetype, ...("aliases" in parser ? parser.aliases : [])]),
    )
    for (const path of [
      "src/main.rs",
      "src/main.cs",
      "src/init.lua",
      "Makefile",
      "src/app.tsx",
    ]) {
      const filetype = filetypeForPath(path)
      expect(filetype).toBeDefined()
      expect(configured.has(filetype!)).toBeTrue()
    }
    for (const path of ["src/Main.kt", "src/query.sql", "src/App.swift", "Dockerfile"]) {
      expect(filetypeForPath(path)).toBeUndefined()
    }
  })

  test("repairs malformed unified-diff counts before structured rendering", () => {
    const rendered = presentableUnifiedDiff(
      "src/main.rs",
      "\u001b[31m@@ -1,9 +1,9 @@\u001b[0m\n-old()\n+new()\n",
    )
    expect(rendered).toBe(
      "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,1 +1,1 @@\n-old()\n+new()\n",
    )
    expect(rendered).not.toContain("\u001b")
    expect(rendered).not.toContain("Error parsing diff")
  })

  test("adds a hunk when a tool returns file headers and changed lines only", () => {
    expect(presentableUnifiedDiff("src/main.rs", [
      "--- a/src/main.rs",
      "+++ b/src/main.rs",
      "-old",
      "+new",
    ].join("\n"))).toBe([
      "--- a/src/main.rs",
      "+++ b/src/main.rs",
      "@@ -1,1 +1,1 @@",
      "-old",
      "+new",
      "",
    ].join("\n"))
  })

  test("turns a headerless pseudo-diff into a safe structured preview", () => {
    const rendered = presentableUnifiedDiff("src/generated.py", [
      "approved full-file change",
      "-print('old')",
      "+print('new')",
    ].join("\n"))
    expect(rendered).toBe([
      "--- a/src/generated.py",
      "+++ b/src/generated.py",
      "@@ -1,2 +1,2 @@",
      " approved full-file change",
      "-print('old')",
      "+print('new')",
      "",
    ].join("\n"))
    expect(rendered).not.toContain("Error parsing diff")
  })

  test("creates truthful changed-lines-only hunks for inline split diffs", () => {
    const rendered = minimalUnifiedDiff("src/main.rs", [
      "--- a/src/main.rs",
      "+++ b/src/main.rs",
      "@@ -10,7 +10,7 @@",
      " unchanged before",
      "-old one",
      "+new one",
      " unchanged gap",
      " unchanged gap two",
      "-old two",
      "+new two",
      " unchanged after",
    ].join("\n"))
    expect(rendered).toBe([
      "--- a/src/main.rs",
      "+++ b/src/main.rs",
      "@@ -11,1 +11,1 @@",
      "-old one",
      "+new one",
      "@@ -14,1 +14,1 @@",
      "-old two",
      "+new two",
      "",
    ].join("\n"))
    expect(rendered).not.toContain("unchanged")
  })

  test("anchors pure inline insertions and deletions on the preceding unchanged line", () => {
    expect(minimalUnifiedDiff(
      "src/insert.rs",
      "--- a/src/insert.rs\n+++ b/src/insert.rs\n@@ -1,2 +1,3 @@\n keep\n+inserted\n tail\n",
    )).toContain("@@ -1,0 +2,1 @@")
    expect(minimalUnifiedDiff(
      "src/delete.rs",
      "--- a/src/delete.rs\n+++ b/src/delete.rs\n@@ -1,3 +1,2 @@\n keep\n-deleted\n tail\n",
    )).toContain("@@ -2,1 +1,0 @@")
  })

  test("measures paired split-diff rows instead of unified wire lines", () => {
    expect(splitDiffVisualRows(
      "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -4,1 +4,1 @@\n-old\n+new\n",
    )).toBe(1)
    expect(splitDiffVisualRows(
      "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -4,2 +4,1 @@\n-old one\n-old two\n+new\n@@ -20,0 +19,1 @@\n+later\n",
    )).toBe(3)
  })

  test("counts changed lines without treating diff metadata as changes", () => {
    expect(diffStats("--- a/src/main.ts\n+++ b/src/main.ts\n@@ -1,2 +1,2 @@\n-old\n+new\n\\ No newline at end of file\n keep\n")).toEqual({ added: 1, removed: 1 })

    const headerLikeContent = "--- a/file\n+++ b/file\n@@ -1 +1 @@\n--- removed-leading-dashes\n+++ added-leading-pluses\n"
    expect(diffStats(headerLikeContent)).toEqual({ added: 1, removed: 1 })
    expect(presentableUnifiedDiff("file", headerLikeContent)).toBe(
      "--- a/file\n+++ b/file\n@@ -1,1 +1,1 @@\n--- removed-leading-dashes\n+++ added-leading-pluses\n",
    )

    const underdeclared = "--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n--- still removed\n+++ still added\n"
    const repaired = presentableUnifiedDiff("file", underdeclared)
    expect(repaired).toBe(
      "--- a/file\n+++ b/file\n@@ -1,2 +1,2 @@\n-old\n+new\n--- still removed\n+++ still added\n",
    )
    expect(diffStats(repaired)).toEqual({ added: 2, removed: 2 })
  })

  test("truncates unified diffs at hunk boundaries and reports hidden body lines", () => {
    const source = "--- a/src/main.ts\n+++ b/src/main.ts\n@@ -1,1 +1,1 @@\n-old\n+new\n@@ -10,1 +10,1 @@\n-before\n+after\n"
    expect(truncateUnifiedDiff(source, 1)).toEqual({
      diff: "--- a/src/main.ts\n+++ b/src/main.ts\n@@ -1,1 +1,1 @@\n-old\n+new\n",
      hiddenLines: 3,
    })
  })

  test("truncates unified diffs with unified-view rows", () => {
    const source = "--- a/src/main.ts\n+++ b/src/main.ts\n@@ -1,1 +1,1 @@\n-old\n+new\n@@ -10,1 +10,1 @@\n-before\n+after\n"
    expect(truncateUnifiedDiff(source, 2, "unified")).toEqual({
      diff: "--- a/src/main.ts\n+++ b/src/main.ts\n@@ -1,1 +1,1 @@\n-old\n+new\n",
      hiddenLines: 3,
    })
  })

  test("truncates an oversized first hunk within the split-view row budget", () => {
    const source = "--- a/src/main.ts\n+++ b/src/main.ts\n@@ -1,3 +1,3 @@\n-old one\n+new one\n-old two\n+new two\n-old three\n+new three\n@@ -10,1 +10,1 @@\n-before\n+after\n"
    const result = truncateUnifiedDiff(source, 1)

    expect(result).toEqual({
      diff: "--- a/src/main.ts\n+++ b/src/main.ts\n@@ -1,1 +1,1 @@\n-old one\n+new one\n",
      hiddenLines: 7,
    })
    expect(splitDiffVisualRows(result.diff)).toBe(1)
    expect(diffStats(result.diff)).toEqual({ added: 1, removed: 1 })
  })

  test("truncates an oversized first hunk within the unified-view row budget", () => {
    const source = "--- a/src/main.ts\n+++ b/src/main.ts\n@@ -4,3 +4,3 @@\n-old one\n+new one\n-old two\n+new two\n-old three\n+new three\n@@ -20,1 +20,1 @@\n-before\n+after\n"
    const result = truncateUnifiedDiff(source, 2, "unified")

    expect(result).toEqual({
      diff: "--- a/src/main.ts\n+++ b/src/main.ts\n@@ -4,1 +4,1 @@\n-old one\n+new one\n",
      hiddenLines: 7,
    })
    expect(splitDiffVisualRows(result.diff)).toBe(1)
    expect(diffStats(result.diff)).toEqual({ added: 1, removed: 1 })
  })

  test("uses OpenCode-compatible fixed scroll speed when configured", () => {
    const fixed = getScrollAcceleration({ scroll_speed: 4 })
    expect(fixed).toBeInstanceOf(CustomSpeedScroll)
    expect(fixed.tick()).toBe(4)
  })

  test("formats compact context counters without confusing them with percentages", () => {
    expect(formatTokenCount("999")).toBe("999")
    expect(formatTokenCount("6400")).toBe("6.4k")
    expect(formatTokenCount("23167")).toBe("23k")
    expect(formatTokenCount("380000")).toBe("380k")
  })

  test("renders subscription usage and AI credits instead of a dead dollar placeholder", () => {
    const usage = {
      input_tokens: "640",
      output_tokens: "96",
      cache_read_tokens: "512",
      cache_write_tokens: "0",
      reasoning_tokens: "12",
    }
    expect(formatCost({ kind: "subscription_quota" }, usage)).toBe("736 tokens")
    expect(formatCost({ kind: "ai_credits", credits_micros: "1250000" }, usage)).toBe(
      "1.250 credits",
    )
    expect(formatSessionCost(null, "6400")).toBe("6400 tokens")
    expect(formatSessionCost({
      utc_day: "2026-01-01",
      subscription_quota: { used: "736", unit: "tokens" },
      session_usage: usage,
      session_cost_micros_usd: "0",
      session_ai_credit_micros: "0",
      session_subscription_tokens: "0",
      daily_cost_micros_usd: "0",
      daily_ai_credit_micros: "0",
      daily_subscription_tokens: "0",
      trailing_minute_cost_micros_usd: "0",
      trailing_minute_ai_credit_micros: "0",
      trailing_minute_subscription_tokens: "0",
      cache_hit_basis_points: 0,
      session_cost_cap_micros_usd: null,
      daily_cost_cap_micros_usd: null,
      session_ai_credit_cap_micros: null,
      daily_ai_credit_cap_micros: null,
      session_token_cap: null,
      daily_token_cap: null,
      spend_rate_alarm_micros_usd_per_minute: null,
      ai_credit_rate_alarm_micros_per_minute: null,
      token_rate_alarm_per_minute: null,
      hard_cap_reached: false,
      session_monetary_accounting_complete: false,
      daily_monetary_accounting_complete: false,
      session_subscription_quota_entries: "1",
      session_cost_unavailable_entries: "0",
      session_non_usd_monetary_entries: "0",
      daily_subscription_quota_entries: "1",
      daily_cost_unavailable_entries: "0",
      daily_non_usd_monetary_entries: "0",
    })).toBe("736 tokens")
  })

  test("summarizes a maximum-size subagent diff before serializing tool output", () => {
    const text = toolOutputText({
      type: "structured",
      value: {
        status: "completed",
        final_text: "done",
        diff_artifact: {
          id: "diff-id",
          base_commit: "0".repeat(40),
          touched_files: Array.from({ length: 4_096 }, (_, index) => ({
            path: `file-${index}.txt`,
            status: "modified",
          })),
          unified_diff: "x".repeat(4 * 1024 * 1024),
        },
      },
    })

    expect(text).toContain("diff-id")
    expect(text).toContain("4194304 chars")
    expect(text).not.toContain("file-4095.txt")
    expect(text.length).toBeLessThan(2_000)
  })

  test("keeps provider-facing tool JSON and internal identifiers out of transcript text", () => {
    const output = {
      type: "mixed" as const,
      parts: [
        { type: "text" as const, text: "README.md" },
        {
          type: "structured" as const,
          value: {
            data: { paths: ["README.md"], machine_local_path: "/private/repo/README.md" },
            stable_prefix_hash: "internal-hash",
            source: "tool_registry",
          },
        },
      ],
    }
    expect(toolOutputText(output)).toBe("README.md")

    const markdown = turnMarkdown({
      role: "tool",
      blocks: [{ type: "tool_result", id: "call-internal", output, is_error: false }],
      meta: { synthetic: false, summary: false },
    })
    expect(markdown).toBe("")
    expect(markdown).not.toContain("machine_local_path")
    expect(markdown).not.toContain("tool_registry")
    expect(markdown).not.toContain("call-internal")
    expect(markdown).not.toContain("{")

    expect(turnMarkdown({
      role: "assistant",
      blocks: [{ type: "thinking", content: "", signature: "opaque-provider-state" }],
      meta: { synthetic: false, summary: false },
    })).toBe("")

    const reasoningTurn = {
      role: "assistant",
      blocks: [{ type: "thinking", content: "**Inspecting**\n\n`Cargo.toml`", signature: null }],
      meta: { synthetic: false, summary: false },
    } satisfies import("../src/protocol").Turn
    expect(turnMarkdown(reasoningTurn)).toBe("")
    expect(turnReasoningMarkdown(reasoningTurn)).toBe("**Inspecting**\n\n`Cargo.toml`")
    expect(turnReasoningMarkdown({
      ...reasoningTurn,
      blocks: [{ type: "thinking", content: " [REDACTED] \n", signature: null }],
    })).toBe("")

    const structuredOnly = toolOutputText({
      type: "structured",
      value: {
        source: "tool_registry",
        kind: "tool_definitions",
        machine_local_path: "/private/repo",
        count: 3,
      },
    })
    expect(structuredOnly).toBe("Count · 3")
    expect(structuredOnly).not.toContain("tool_registry")
    expect(structuredOnly).not.toContain("tool_definitions")
    expect(structuredOnly).not.toContain("/private/repo")

    expect(toolOutputText({
      type: "text",
      text: "{\n  \"name\": \"user-authored.json\"\n}",
    })).toContain('"name": "user-authored.json"')
  })

})
