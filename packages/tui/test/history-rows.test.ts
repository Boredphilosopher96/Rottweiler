import { expect, test } from "bun:test"
import { createTestRenderer, MockTreeSitterClient } from "@opentui/core/testing"
import { createRottweilerApp } from "../src/app"
import { conversationItem, historyReaderFor } from "./fixtures/history"
import type { TranscriptItem } from "../src/protocol"

function toolItem(): TranscriptItem {
  return {
    id: "2", ordinal: "1", revision: "2", agent_turn: "1",
    content: {
      type: "tool", invocation_id: "invocation", name: "bash", call_index: 0,
      arguments: {
        text: '{"command":"echo hello"}', format: "json", complete: true,
        source: { sequence: "2", selector: { type: "tool_arguments" } }
      },
      diff: null, status: { type: "running" },
    },
  }
}

test("native tool row keeps identity, expansion and selection across final revision and resize", async () => {
  const harness = await createTestRenderer({ width: 90, height: 25, useThread: false })
  const item = toolItem()
  const items = [conversationItem(1, "assistant", "Inspecting output"), item]
  const app = createRottweilerApp(harness.renderer, { sessionId: "history", historyReader: historyReaderFor(items), treeSitterClient: new MockTreeSitterClient() })
  harness.renderer.root.add(app)
  try {
    await Bun.sleep(0)
    await harness.renderOnce()
    const row = app.transcript.mountedCards.get("2")
    if (row === undefined || item.content.type !== "tool") throw new Error("missing tool row")
    app.transcript.selectNextBlock()
    app.transcript.toggleSelectedBlock()
    expect(app.transcript.selectedBlockId).toBe("tool:invocation")
    expect(row.expanded).toBe(true)
    const markdown = row.markdown
    items[1] = {
      ...item, revision: "3", content: {
        ...item.content,
        status: {
          type: "finished", presentation: null, is_error: false, output: {
            text: "hello", format: "text", complete: true,
            source: { sequence: "3", selector: { type: "tool_output" } }
          }
        },
      }
    }
    app.transcript.scrollTo(app.transcript.scroller.scrollHeight)
    await Bun.sleep(0)
    await harness.renderOnce()
    expect(app.transcript.mountedCards.get("2")).toBe(row)
    expect(row.markdown).toBe(markdown)
    expect(row.expanded).toBe(true)
    expect(app.transcript.selectedBlockId).toBe("tool:invocation")
    expect(row.markdown.content).toContain("hello")
    app.width = 70
    await harness.renderOnce()
    expect(app.transcript.mountedCards.get("2")).toBe(row)
    expect(row.expanded).toBe(true)
  } finally { app.destroy(); harness.renderer.destroy() }
})

test("reasoning and tool rows remain separate keyboard blocks in visual order", async () => {
  const harness = await createTestRenderer({ width: 90, height: 25, useThread: false })
  const app = createRottweilerApp(harness.renderer, {
    historyReader: historyReaderFor([
      conversationItem(1, "assistant", "Answer", "Inspect the source first."), toolItem(),
    ]), treeSitterClient: new MockTreeSitterClient()
  })
  harness.renderer.root.add(app)
  try {
    await Bun.sleep(0)
    await harness.renderOnce()
    app.transcript.selectNextBlock()
    expect(app.transcript.selectedBlockId).toBe("history-reasoning:1")
    app.transcript.toggleSelectedBlock()
    expect(app.transcript.mountedCards.get("1")?.reasoning.expanded).toBe(false)
    app.transcript.selectNextBlock()
    expect(app.transcript.selectedBlockId).toBe("tool:invocation")
    app.transcript.selectNextBlock()
    expect(app.transcript.selectedBlockId).toBe("tool:invocation")
    app.transcript.selectPreviousBlock()
    expect(app.transcript.selectedBlockId).toBe("history-reasoning:1")
  } finally { app.destroy(); harness.renderer.destroy() }
})
