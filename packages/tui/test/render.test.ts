import { describe, expect, test } from "bun:test"

import { formatCost, formatSessionCost, formatTokenCount, toolOutputText, TranscriptVirtualizer, turnMarkdown, turnReasoningMarkdown } from "../src/render"

describe("bounded retained rendering", () => {
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
      turns: [{
        turn_id: "1",
        attribution: "main",
        usage,
        cost: { kind: "subscription_quota", used: "736", unit: "tokens" },
      }],
      session_usage: usage,
      session_cost_micros_usd: "0",
      session_ai_credit_micros: "0",
      daily_cost_micros_usd: "0",
      daily_ai_credit_micros: "0",
      trailing_minute_cost_micros_usd: "0",
      trailing_minute_ai_credit_micros: "0",
      cache_hit_basis_points: 0,
      session_cost_cap_micros_usd: null,
      daily_cost_cap_micros_usd: null,
      session_ai_credit_cap_micros: null,
      daily_ai_credit_cap_micros: null,
      spend_rate_alarm_micros_usd_per_minute: null,
      ai_credit_rate_alarm_micros_per_minute: null,
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
    expect(structuredOnly).toBe("Count: 3")
    expect(structuredOnly).not.toContain("tool_registry")
    expect(structuredOnly).not.toContain("tool_definitions")
    expect(structuredOnly).not.toContain("/private/repo")
  })

  test("includes bounded child-panel rows in transcript virtual offsets", () => {
    const virtualizer = new TranscriptVirtualizer(0)
    const entries = [
      {
        sequenceId: "1",
        agentTurn: "1",
        turn: {
          role: "assistant" as const,
          blocks: [{ type: "text" as const, text: "first" }],
          meta: { synthetic: false, summary: false },
        },
      },
      {
        sequenceId: "2",
        agentTurn: "2",
        turn: {
          role: "assistant" as const,
          blocks: [{ type: "text" as const, text: "second" }],
          meta: { synthetic: false, summary: false },
        },
      },
    ]
    virtualizer.update(entries, 80, (entry) => (entry.agentTurn === "1" ? 11 : 0))

    expect(virtualizer.heightAt(0) - virtualizer.heightAt(1)).toBe(11)
    expect(virtualizer.window(0, 1).totalHeight).toBe(
      virtualizer.heightAt(0) + virtualizer.heightAt(1),
    )
  })

  test("uses only retained tool rows for a tool-only transcript entry", () => {
    const virtualizer = new TranscriptVirtualizer(0)
    const entries = [{
      sequenceId: "1",
      agentTurn: "1",
      turn: {
        role: "tool" as const,
        blocks: [{
          type: "tool_result" as const,
          id: "read-1",
          output: { type: "text" as const, text: "README" },
          is_error: false,
        }],
        meta: { synthetic: false, summary: false },
      },
    }]

    virtualizer.update(entries, 80, () => 6)

    expect(virtualizer.heightAt(0)).toBe(6)
    expect(virtualizer.window(0, 1).totalHeight).toBe(6)
  })
})
