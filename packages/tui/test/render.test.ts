import { describe, expect, test } from "bun:test"

import { formatCost, formatSessionCost, formatTokenCount, terminalMarkdown, toolOutputText, TranscriptVirtualizer, turnMarkdown, turnReasoningMarkdown } from "../src/render"

describe("bounded retained rendering", () => {
  test("renders completed Mermaid and flowchart fences as terminal diagrams without flashing source", () => {
    const rendered = terminalMarkdown([
      "## Architecture",
      "",
      "```mermaid",
      "flowchart LR",
      "  A[Client] --> B[Engine]",
      "```",
    ].join("\n"), 80)

    expect(rendered).toContain("Architecture")
    expect(rendered).toContain("Client")
    expect(rendered).toContain("Engine")
    expect(rendered).toMatch(/[┌┐└┘─│►]/)
    expect(rendered).not.toContain("flowchart LR")
    expect(rendered).not.toContain("```mermaid")

    const flowchart = terminalMarkdown([
      "```flowchart",
      "graph TD",
      "  Start --> Done",
      "```",
    ].join("\n"), 60)
    expect(flowchart).toContain("Start")
    expect(flowchart).toContain("Done")
  })

  test("keeps incomplete and invalid Mermaid definitions user-facing", () => {
    const streaming = terminalMarkdown("before\n\n```mermaid\nflowchart TB\n A -->", 80, "streaming")
    expect(streaming).toContain("Rendering diagram")
    expect(streaming).not.toContain("flowchart TB")
    expect(streaming).not.toContain("A -->")

    const invalid = terminalMarkdown("```mermaid\nnot a diagram\n```", 80)
    expect(invalid).toContain("Diagram could not be rendered")
    expect(invalid).not.toContain("not a diagram")

    const committed = terminalMarkdown("```mermaid\nflowchart TB\n A -->", 80)
    expect(committed).toContain("closing fence is missing")
    expect(committed).not.toContain("Rendering diagram")
  })

  test("supports CommonMark tilde fences and leaves indented code literal", () => {
    const tilde = terminalMarkdown("~~~mermaid\nflowchart LR\n A --> B\n~~~", 60)
    expect(tilde).toMatch(/[┌┐└┘─│►]/)
    expect(tilde).not.toContain("flowchart LR")

    const indented = terminalMarkdown("    ```mermaid\n    flowchart LR\n    A --> B\n    ```", 60)
    expect(indented).toContain("```mermaid")
    expect(indented).toContain("flowchart LR")

    const titledTilde = terminalMarkdown("~~~mermaid title=`architecture`\nflowchart LR\n A --> B\n~~~", 60)
    expect(titledTilde).toMatch(/[┌┐└┘─│►]/)
    expect(titledTilde).not.toContain("flowchart LR")

    const quoted = terminalMarkdown("> ```mermaid\n> flowchart LR\n> A --> B\n> ```", 60)
    expect(quoted).toContain("> ```text")
    expect(quoted).not.toContain("flowchart LR")

    const listed = terminalMarkdown("- Architecture\n  ```mermaid\n  flowchart LR\n  A --> B\n  ```", 60)
    expect(listed).toContain("  ```text")
    expect(listed).not.toContain("flowchart LR")

    const outsideClose = terminalMarkdown("> ```mermaid\n> flowchart LR\n> A --> B\n```\nafter", 100)
    expect(outsideClose).toContain("closing fence is missing")
    expect(outsideClose).toContain("```\nafter")
    expect(outsideClose).not.toContain("```text")
  })

  test("rejects adversarial graph complexity quickly and bounds output width", () => {
    const source = `\`\`\`mermaid\nflowchart LR\n${Array.from({ length: 50 }, (_, index) => `N${index} --> N${index + 1}`).join("\n")}\n\`\`\``
    const started = performance.now()
    const rejected = terminalMarkdown(source, 80)
    expect(performance.now() - started).toBeLessThan(16)
    expect(rejected).toContain("too large to render safely")

    const wide = terminalMarkdown("```mermaid\nflowchart LR\n A --> B --> C --> D --> E --> F\n```", 40)
    const body = wide.split("\n").slice(1, -1)
    expect(Math.max(...body.map((line) => Bun.stringWidth(line)))).toBeLessThanOrEqual(40)

    for (const width of [20, 24]) {
      const narrow = terminalMarkdown("```mermaid\nflowchart LR\n A[界面] --> B[核心引擎]\n```", width)
      expect(Math.max(...narrow.split("\n").map((line) => Bun.stringWidth(line)))).toBeLessThanOrEqual(width)
    }
  })

  test("rejects grouped-node expansion and bounds aggregate diagram work", () => {
    const group = Array.from({ length: 80 }, (_, index) => `N${index}`).join(" & ")
    const grouped = `\`\`\`mermaid\nflowchart LR\n${group} --> ${group}\n\`\`\``
    const groupedStart = performance.now()
    expect(terminalMarkdown(grouped, 80)).toContain("too large to render safely")
    expect(performance.now() - groupedStart).toBeLessThan(16)

    const diagram = "```mermaid\nflowchart LR\n A --> B --> C\n```"
    const response = Array.from({ length: 20 }, () => diagram).join("\n")
    const responseStart = performance.now()
    const rendered = terminalMarkdown(response, 80)
    expect(performance.now() - responseStart).toBeLessThan(33)
    expect(rendered).toContain("Additional diagrams were omitted")

    const narrowOmissions = terminalMarkdown(response, 20)
    expect(Math.max(...narrowOmissions.split("\n").map((line) => Bun.stringWidth(line)))).toBeLessThanOrEqual(20)

    const nested = terminalMarkdown("> > ```mermaid\n> > flowchart LR\n> > A[界面] --> B[核心]\n> > ```", 20)
    expect(Math.max(...nested.split("\n").map((line) => Bun.stringWidth(line)))).toBeLessThanOrEqual(20)

    const deepPrefix = "> ".repeat(8)
    const deep = terminalMarkdown(`${deepPrefix}\`\`\`mermaid\n${deepPrefix}flowchart LR\n${deepPrefix}A --> B\n${deepPrefix}\`\`\``, 20)
    expect(Math.max(...deep.split("\n").map((line) => Bun.stringWidth(line)))).toBeLessThanOrEqual(20)
    expect(deep).toContain("Dia")
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
