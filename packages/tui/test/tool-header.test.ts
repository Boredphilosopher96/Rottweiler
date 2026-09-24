import { afterEach, expect, test } from "bun:test"
import { SyntaxStyle } from "@opentui/core"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { ToolBlockRenderable } from "../src/components/transcript/blocks"
import { TranscriptRowRenderable } from "../src/components/transcript/row"
import { historicalToolPresentation } from "../src/render/tool-header"
import { prepareToolDisplay } from "../src/state/tool-display"
import { toolOutputBuffer } from "../src/state/display-buffer"
import { kennelTheme } from "../src/theme"
import { toolItem } from "./fixtures/history"

let renderer: TestRenderer | undefined
afterEach(() => { renderer?.destroy(); renderer = undefined })

for (const name of ["bash", "read"]) {
  test(`live and durable ${name} headers show the same subject and completion`, async () => {
    const setup = await createTestRenderer({ width: 90, height: 24, useThread: false })
    renderer = setup.renderer
    const args = name === "bash" ? { command: "cargo test --workspace" } : { path: "src/lib.rs" }
    const item = toolItem(1, name, JSON.stringify(args), "Completed output")
    const style = SyntaxStyle.create()
    const live = new ToolBlockRenderable(renderer, kennelTheme, {
      name, args, status: "finished", isError: false,
      display: prepareToolDisplay({ type: "text", text: "Completed output" }, null, args, false),
      invocationId: "live", toolCallId: "live", turnId: "1", capabilities: [], rationale: null,
      diff: null, diffSource: null, chunks: toolOutputBuffer([]), source: null, callIndex: 0, timing: { kind: "unknown" },
    }, false, undefined, { syntaxStyle: style })
    const history = new TranscriptRowRenderable(renderer, kennelTheme, item, { syntaxStyle: style, onExpansionChange() {} })
    history.update(item, 90)
    renderer.root.add(live)
    renderer.root.add(history)
    await setup.renderOnce()
    expect(history.header.plainText).toBe(live.header.plainText)
    expect(history.header.plainText).toContain(name === "bash" ? "cargo test --workspace" : "src/lib.rs")
    expect(history.header.plainText).toContain("✓ Completed")
    history.toggle()
    expect(history.markdown.content).not.toContain('"command":')
  })
}

test("history never interprets truncated or malformed argument previews", () => {
  const item = toolItem(1, "bash", '{"command":"delete everything"}', "ok")
  if (item.content.type !== "tool") throw new Error("tool fixture")
  const truncated = { ...item.content, arguments: { ...item.content.arguments, complete: false } }
  expect(historicalToolPresentation(truncated).args).toBeNull()
  expect(historicalToolPresentation(truncated).display?.subject).toBe("arguments truncated")
  const malformed = { ...item.content, arguments: { ...item.content.arguments, text: '{"command":' } }
  expect(historicalToolPresentation(malformed).display?.subject).toBe("arguments unavailable")
  const oversized = { ...item.content, arguments: { ...item.content.arguments, text: JSON.stringify({ command: "x".repeat(5000) }) } }
  expect(historicalToolPresentation(oversized).args).toBeNull()
})
