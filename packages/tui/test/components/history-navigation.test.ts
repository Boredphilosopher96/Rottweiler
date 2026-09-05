import { afterEach, describe, expect, test } from "bun:test"
import { createTestRenderer, MockTreeSitterClient, type TestRenderer } from "@opentui/core/testing"
import type { BaseRenderable } from "@opentui/core"
import { createRottweilerApp } from "../../src/app"
import { ToolBlockRenderable } from "../../src/components"
import { createInitialState, type ToolProjection } from "../../src/state"
import { createStreamingTail } from "../../src/state/model"
import { toolOutputBuffer } from "../../src/state/display-buffer"
import type { TranscriptItem } from "../../src/protocol"
import { commandItem, conversationItem, sessionReaderFor, toolItem, waitForHistory } from "../fixtures/history"
import { meta } from "./fixtures"

function liveTool(): ToolProjection {
  return {
    toolCallId: "provider-call", invocationId: "host-invocation", turnId: "1", name: "read",
    args: { path: "README.md" }, status: "running", capabilities: [], rationale: null, diff: null,
    chunks: toolOutputBuffer([]), display: null, source: null, isError: false, callIndex: 0, timing: { kind: "unknown" }
  }
}
function liveBlocks(root: BaseRenderable): ToolBlockRenderable[] {
  if (root instanceof ToolBlockRenderable) return [root]
  return root.getChildren().flatMap(liveBlocks)
}
function blockItems(): TranscriptItem[] {
  return [conversationItem(1, "assistant", "", "Inspect both files."),
  toolItem(2, "read", '{"path":"first.txt"}', "first output"),
  toolItem(3, "read", '{"path":"second.txt"}', "second output")]
}

describe("semantic history navigation", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => { renderer?.destroy(); renderer = undefined })

  test("one invocation transfers from live output to its canonical row with expansion and selection intact", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const items: TranscriptItem[] = []
    const tool = liveTool()
    const app = createRottweilerApp(renderer, {
      sessionId: "session-components", sessionReader: sessionReaderFor(items),
      treeSitterClient: new MockTreeSitterClient(), initialState: {
        ...createInitialState(), tools: { [tool.invocationId]: tool },
        streamingTail: createStreamingTail({ turnId: "1", text: "", thinking: "", citations: [], toolInvocationIds: [tool.invocationId], finished: null })
      }
    })
    renderer.root.add(app)
    await setup.flush()
    app.transcript.selectNextBlock()
    app.transcript.toggleSelectedBlock()
    expect(app.transcript.selectedBlockId).toBe("tool:host-invocation")
    expect(liveBlocks(app.transcript.streamingCard)).toHaveLength(1)
    const final = toolItem(2, "read", '{"path":"README.md"}', "canary output")
    if (final.content.type !== "tool") throw new Error("tool fixture")
    final.content.invocation_id = tool.invocationId
    final.revision = "3"
    items.push(final)
    app.handleEvent({
      type: "tool_call_finished", presentation: null, meta: meta("3"), turn_id: "1", tool_call_id: tool.toolCallId,
      invocation_id: tool.invocationId, output: { type: "text", text: "canary output" }, is_error: false, call_index: 0
    })
    await waitForHistory(setup, () => app.transcript.mountedCards.has("2"))
    const row = app.transcript.mountedCards.get("2")
    expect(liveBlocks(app.transcript.streamingCard)).toHaveLength(0)
    expect(row?.expanded).toBe(true)
    expect(row?.markdown.content).toContain("canary output")
    expect(app.transcript.selectedBlockId).toBe("tool:host-invocation")
    app.transcript.toggleSelectedBlock()
    expect(row?.expanded).toBe(false)
    expect(row?.markdown.visible).toBe(false)
  })

  test("reasoning and tools navigate in semantic order without wrapping", async () => {
    const setup = await createTestRenderer({ width: 90, height: 24, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: sessionReaderFor(blockItems()), treeSitterClient: new MockTreeSitterClient() })
    renderer.root.add(app)
    await setup.flush()
    const order: Array<string | null> = []
    for (let index = 0; index < 4; index++) { app.transcript.selectNextBlock(); order.push(app.transcript.selectedBlockId) }
    expect(order).toEqual(["history-reasoning:1", "tool:invocation-2", "tool:invocation-3", "tool:invocation-3"])
    app.transcript.selectPreviousBlock()
    expect(app.transcript.selectedBlockId).toBe("tool:invocation-2")
    app.transcript.selectPreviousBlock()
    app.transcript.selectPreviousBlock()
    expect(app.transcript.selectedBlockId).toBe("history-reasoning:1")
  })

  test("live reasoning and tool invocations follow the historical blocks", async () => {
    const setup = await createTestRenderer({ width: 90, height: 24, useThread: false })
    renderer = setup.renderer
    const tool = liveTool()
    const app = createRottweilerApp(renderer, {
      sessionReader: sessionReaderFor(blockItems()), treeSitterClient: new MockTreeSitterClient(),
      initialState: {
        ...createInitialState(), tools: { [tool.invocationId]: tool }, streamingTail: createStreamingTail({
          turnId: "2", text: "", thinking: "Next inspection", citations: [], toolInvocationIds: [tool.invocationId], finished: null
        })
      }
    })
    renderer.root.add(app)
    await setup.flush()
    const order: Array<string | null> = []
    for (let index = 0; index < 5; index++) { app.transcript.selectNextBlock(); order.push(app.transcript.selectedBlockId) }
    expect(order).toEqual(["history-reasoning:1", "tool:invocation-2", "tool:invocation-3", "reasoning:tail:2", "tool:host-invocation"])
  })

  test("selection reveals an off-screen header and repeated selection leaves the viewport still", async () => {
    const setup = await createTestRenderer({ width: 90, height: 18, useThread: false })
    renderer = setup.renderer
    const items = [...Array.from({ length: 15 }, (_, index) => commandItem(index, "fixture", "Leading history")), toolItem(15, "read", "{}")]
    const app = createRottweilerApp(renderer, { sessionReader: sessionReaderFor(items), treeSitterClient: new MockTreeSitterClient() })
    renderer.root.add(app)
    await setup.flush()
    app.transcript.setScrollOffset(0)
    await setup.flush()
    app.transcript.selectNextBlock()
    await setup.flush()
    expect(app.transcript.selectedBlockId).toBe("tool:invocation-15")
    expect(app.transcript.scroller.scrollTop).toBeGreaterThan(0)
    const position = app.transcript.scroller.scrollTop
    app.transcript.selectNextBlock()
    expect(app.transcript.scroller.scrollTop).toBe(position)
  })

  test("mounted source revisions preserve the row, parser and user expansion through resize", async () => {
    const setup = await createTestRenderer({ width: 90, height: 24, useThread: false })
    renderer = setup.renderer
    const items = [toolItem(1, "read", "{}")]
    const app = createRottweilerApp(renderer, { sessionReader: sessionReaderFor(items), treeSitterClient: new MockTreeSitterClient() })
    renderer.root.add(app)
    await setup.flush()
    app.transcript.selectNextBlock()
    app.transcript.toggleSelectedBlock()
    const row = app.transcript.mountedCards.get("1")
    const markdown = row?.markdown
    items[0] = { ...toolItem(1, "read", "{}", "complete output"), revision: "2" }
    app.transcript.scrollTo(0)
    await waitForHistory(setup, () => app.transcript.mountedCards.get("1")?.item.revision === "2")
    setup.resize(60, 18)
    await setup.flush()
    expect(app.transcript.mountedCards.get("1")).toBe(row)
    expect(row?.markdown).toBe(markdown)
    expect(row?.expanded).toBe(true)
    expect(app.transcript.selectedBlockId).toBe("tool:invocation-1")
    expect(row?.markdown.content).toContain("complete output")
  })

  test("expansion returns when a bounded window remounts the same source invocation", async () => {
    const setup = await createTestRenderer({ width: 90, height: 24, useThread: false })
    renderer = setup.renderer
    const items = Array.from({ length: 60 }, (_, index) => toolItem(index, "read", "{}"))
    const app = createRottweilerApp(renderer, { sessionReader: sessionReaderFor(items), treeSitterClient: new MockTreeSitterClient() })
    renderer.root.add(app)
    await setup.flush()
    const row = app.transcript.mountedCards.get("59")
    row?.toggle()
    app.transcript.scrollTo(0)
    await waitForHistory(setup, () => app.transcript.mountedCards.has("0"))
    expect(app.transcript.mountedCards.has("59")).toBe(false)
    app.transcript.scrollTo(Infinity)
    await waitForHistory(setup, () => app.transcript.mountedCards.has("59"))
    expect(app.transcript.mountedCards.get("59")).not.toBe(row)
    expect(app.transcript.mountedCards.get("59")?.expanded).toBe(true)
    expect(app.transcript.mountedCards.size).toBeLessThanOrEqual(16)
  })

  test("removing the selected source row clears selection without selecting another tool", async () => {
    const setup = await createTestRenderer({ width: 90, height: 24, useThread: false })
    renderer = setup.renderer
    const items = [toolItem(1, "read", "{}")]
    let generation = "0"
    const app = createRottweilerApp(renderer, { sessionReader: sessionReaderFor(items, () => ({ generation, through: "2" })), treeSitterClient: new MockTreeSitterClient() })
    renderer.root.add(app)
    await setup.flush()
    app.transcript.selectNextBlock()
    items.length = 0
    generation = "1"
    app.transcript.scrollTo(0)
    await waitForHistory(setup, () => app.transcript.mountedCards.size === 0)
    expect(app.transcript.selectedBlockId).toBeNull()
  })
})
