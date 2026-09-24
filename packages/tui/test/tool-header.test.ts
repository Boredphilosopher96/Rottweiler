import { afterEach, expect, test } from "bun:test"
import { SyntaxStyle } from "@opentui/core"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { ToolBlockRenderable } from "../src/components/transcript/blocks"
import { TranscriptRowRenderable } from "../src/components/transcript/row"
import { toolHeaderContent, toolSummary } from "../src/render/tool-header"
import { transcriptToolRow, liveToolRow } from "../src/render/tool-row"
import { prepareToolDisplay } from "../src/state/tool-display"
import { toolOutputBuffer } from "../src/state/display-buffer"
import type { ToolProjection } from "../src/state"
import { kennelTheme } from "../src/theme"
import { toolItem } from "./fixtures/history"

let renderer: TestRenderer | undefined
afterEach(() => { renderer?.destroy(); renderer = undefined })

function live(name: string, args: Record<string, unknown>, output = "Completed output", overrides: Partial<ToolProjection> = {}): ToolProjection {
  return {
    name, args, status: "finished", isError: false,
    display: prepareToolDisplay({ type: "text", text: output }, null, args, false),
    invocationId: "live", toolCallId: "live", turnId: "1", capabilities: [], rationale: null,
    diff: null, diffSource: null, chunks: toolOutputBuffer([]), source: null, callIndex: 0, timing: { kind: "unknown" },
    ...overrides,
  }
}

for (const name of ["bash", "read"]) {
  test(`live and restored ${name} rows render through one renderer with the same header`, async () => {
    const setup = await createTestRenderer({ width: 90, height: 24, useThread: false })
    renderer = setup.renderer
    const args = name === "bash" ? { command: "cargo test --workspace" } : { path: "src/lib.rs" }
    const item = toolItem(1, name, JSON.stringify(args), "Completed output")
    const style = SyntaxStyle.create()
    const card = new ToolBlockRenderable(renderer, kennelTheme, live(name, args), false, undefined, { syntaxStyle: style })
    const history = new TranscriptRowRenderable(renderer, kennelTheme, item, { syntaxStyle: style, onExpansionChange() {} })
    history.update(item, 90)
    renderer.root.add(card)
    renderer.root.add(history)
    await setup.renderOnce()
    expect(history.tool).toBeInstanceOf(ToolBlockRenderable)
    expect(history.header.plainText).toBe(card.header.plainText)
    expect(history.header.plainText).toStartWith(name === "bash" ? "● Bash cargo test --workspace" : "● Read src/lib.rs")
    expect(history.header.plainText.trimEnd()).toEndWith("✓")
    expect(history.header.plainText).not.toContain("=")
    history.toggle()
    await setup.renderOnce()
    expect(history.tool?.body.plainText).toContain("Completed output")
    expect(history.tool?.body.plainText).not.toContain('"command":')
    expect(setup.captureCharFrame()).not.toContain("Open content")
  })
}

test("restored rows never interpret truncated or malformed argument previews", () => {
  const item = toolItem(1, "bash", '{"command":"delete everything"}', "ok")
  if (item.content.type !== "tool") throw new Error("tool fixture")
  const truncated = transcriptToolRow({ ...item.content, arguments: { ...item.content.arguments, complete: false } })
  expect(truncated.args).toBeNull()
  expect(toolSummary(truncated).subject).toBe("arguments truncated")
  const malformed = transcriptToolRow({ ...item.content, arguments: { ...item.content.arguments, text: '{"command":' } })
  expect(toolSummary(malformed).subject).toBe("arguments unavailable")
  const oversized = transcriptToolRow({ ...item.content, arguments: { ...item.content.arguments, text: JSON.stringify({ command: "x".repeat(5000) }) } })
  expect(oversized.args).toBeNull()
})

test("built-in tools have humanized one-line summaries", () => {
  const summary = (name: string, args: Record<string, unknown>, output = "") => {
    const value = toolSummary(liveToolRow(live(name, args, output)))
    return [value.verb, value.subject, value.detail].filter(Boolean).join(" ")
  }
  expect(summary("read", { path: "calc.py" })).toBe("Read calc.py")
  expect(summary("read", { path: "calc.py", start_line: 10, line_count: 5 })).toBe("Read calc.py lines 10–14")
  expect(summary("bash", { command: "python test_calc.py", cwd: "." })).toBe("Bash python test_calc.py")
  expect(summary("bash", { command: "set -e\nmake" })).toBe("Bash set -e …")
  expect(summary("grep", { pattern: "foo", path: "src/" }, "Matches · 12")).toBe('Search "foo" in src/ 12 matches')
  expect(summary("glob", { pattern: "**/*.ts" }, "Files · 1")).toBe('Find "**/*.ts" 1 file')
  expect(summary("ls", { path: "src" })).toBe("List src")
  expect(summary("webfetch", { url: "https://example.com/a" })).toBe("Fetch https://example.com/a")
  expect(summary("websearch", { query: "bun test" })).toBe('Search web "bun test"')
  expect(summary("todo", { action: "replace", items: [{}, {}] })).toBe("Update todos 2 items")
  expect(summary("skill", { name: "review" })).toBe("Skill review")
  expect(summary("spawn_agent", { action: "spawn", task: "Audit the parser\nthen report", agent: "explore" })).toBe('Agent explore "Audit the parser"')
  expect(summary("spawn_agent", { action: "wait", ids: ["a", "b"] })).toBe("Wait for agents 2 agents")
  expect(summary("mcp__github__create_issue", { title: "Broken build", body: "details" })).toBe("github · create_issue Broken build")
  expect(summary("mcp_call", { server: "docs", name: "search", arguments: { query: "diff" } })).toBe("docs · search diff")
  expect(summary("custom_tool", { target: "src/main.rs", mode: "fast" })).toBe("custom_tool src/main.rs")
  expect(summary("custom_tool", { api_key: "secret" })).toBe("custom_tool")
})

test("edits show diff counts, and a pending approval keeps the row compact", async () => {
  const setup = await createTestRenderer({ width: 90, height: 24, useThread: false })
  renderer = setup.renderer
  const diff = {
    proposal_id: "p", path: "calc.py", arguments_hash: "a", base_hash: "b", diff_hash: "d", truncated: false,
    unified_diff: "--- a/calc.py\n+++ b/calc.py\n@@ -1,3 +1,3 @@\n def add(a, b):\n-    return a - b\n+    return a + b\n \n",
  }
  const pending = live("edit", { path: "calc.py", old: "a - b", new: "a + b" }, "", { status: "awaiting_approval", display: null, diff })
  const card = new ToolBlockRenderable(renderer, kennelTheme, pending, undefined, undefined, { syntaxStyle: SyntaxStyle.create() })
  renderer.root.add(card)
  await setup.renderOnce()
  expect(card.expanded).toBeFalse()
  expect(card.header.plainText).toStartWith("● Edit calc.py  +1 −1")
  expect(card.header.plainText).toContain("? awaiting approval")
  expect(card.diff === null || !card.diff.visible || !card.expanded).toBeTrue()
  expect(setup.captureCharFrame()).not.toContain("return a - b")
  card.update({ ...pending, status: "finished", isError: false, display: prepareToolDisplay({ type: "text", text: "Edited calc.py" }, null, pending.args, false) })
  await setup.renderOnce()
  expect(card.expanded).toBeTrue()
  expect(card.header.plainText.trimEnd()).toEndWith("✓")
  expect(setup.captureCharFrame()).toContain("return a + b")
})

test("durations appear only above one second and failures state the outcome", () => {
  const started = 1_000
  const header = (tool: ToolProjection) => toolHeaderContent(liveToolRow(tool), 90, kennelTheme, started + 10_000)
    .chunks.map(chunk => chunk.text).join("")
  expect(header(live("read", { path: "a" }, "", { timing: { kind: "closed", startedAtMs: started, finishedAtMs: started + 400 } })).trimEnd()).toEndWith("✓")
  expect(header(live("bash", { command: "sleep 2" }, "", { timing: { kind: "closed", startedAtMs: started, finishedAtMs: started + 2_300 } })).trimEnd()).toEndWith("✓ 2.3s")
  expect(header(live("bash", { command: "sleep 9" }, "", { status: "running", display: null, timing: { kind: "open", startedAtMs: started, lastObservedAtMs: started } }))).toContain(" 10.0s")
  const failed = live("bash", { command: "npm test" }, "", {
    isError: true, display: prepareToolDisplay({ type: "text", text: "tests failed" }, null, { command: "npm test" }, true),
  })
  expect(toolSummary(liveToolRow(failed)).detail).toBe("tests failed")
  expect(header(failed)).toContain("✗")
})
