import { afterEach, describe, expect, test } from "bun:test"
import { createTestRenderer, MockTreeSitterClient, type TestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import { ReasoningBlockRenderable } from "../../src/components"
import type { TranscriptItem } from "../../src/protocol"
import { createInitialState, type RottweilerState } from "../../src/state"
import { createStreamingTail } from "../../src/state/model"
import { commandItem, conversationItem, sessionReaderFor, toolItem, waitForHistory } from "../fixtures/history"
import { neverUsage, permissionState } from "./fixtures"

let renderer: TestRenderer | undefined
async function fixture(items: TranscriptItem[], state = createInitialState(), height = 24) {
  const setup = await createTestRenderer({ width: 90, height, useThread: false })
  renderer = setup.renderer
  const app = createRottweilerApp(renderer, {
    sessionReader: sessionReaderFor(items), initialState: state,
    treeSitterClient: new MockTreeSitterClient({ autoResolveTimeout: 0 })
  })
  renderer.root.add(app)
  await setup.flush()
  return { setup, app }
}

describe("semantic history rendering", () => {
  afterEach(() => { renderer?.destroy(); renderer = undefined })

  test("historical rows and the streaming parser retain identity through an unrelated live update", async () => {
    const { app, setup } = await fixture(Array.from({ length: 120 }, (_, index) => conversationItem(index + 1, "assistant", `Answer ${index}`)))
    const rows = [...app.transcript.mountedCards.values()]
    const markdown = app.transcript.streamingMarkdown
    app.setState({ ...app.state, streamingTail: createStreamingTail({ turnId: "121", text: "A live answer", thinking: "", citations: [], toolInvocationIds: [], finished: null }) })
    await setup.flush()
    expect(app.transcript.streamingMarkdown).toBe(markdown)
    expect([...app.transcript.mountedCards.values()]).toEqual(rows)
    expect(app.transcript.mountedKeys).toEqual(Array.from({ length: 16 }, (_, index) => String(index + 105)))
  })

  test("large source history stays outside the live reducer and only its bounded page mounts", async () => {
    const { app } = await fixture(Array.from({ length: 600 }, (_, index) => conversationItem(index + 1, "assistant", `Durable answer ${index}`)))
    expect("transcript" in app.state).toBe(false)
    expect(app.transcript.mountedEntryCount).toBe(16)
    expect(app.transcript.mountedKeys.at(-1)).toBe("600")
  })

  test("append refreshes retain overlapping source rows and destroy rows leaving the window", async () => {
    const items = Array.from({ length: 40 }, (_, index) => conversationItem(index + 1, "assistant", `Answer ${index}`))
    const { app, setup } = await fixture(items)
    const departed = app.transcript.mountedCards.get("25")
    const retained = app.transcript.mountedCards.get("40")
    items.push(conversationItem(41, "assistant", "New answer"))
    app.transcript.scrollTo(Infinity)
    await waitForHistory(setup, () => app.transcript.mountedCards.has("41"))
    expect(app.transcript.mountedCards.get("40")).toBe(retained)
    expect(departed?.isDestroyed).toBe(true)
    expect(app.transcript.mountedEntryCount).toBe(16)
  })

  test("an identical structured command page retains the row and parser", async () => {
    const item = commandItem(1, "status", '{"queue":"empty"}')
    if (item.content.type !== "command") throw new Error("command fixture")
    item.content.message.format = "json"
    const { app, setup } = await fixture([item])
    const row = app.transcript.mountedCards.get("1")
    const markdown = row?.markdown
    app.transcript.scrollTo(0)
    await setup.renderOnce()
    await setup.flush()
    expect(app.transcript.mountedCards.get("1")).toBe(row)
    expect(row?.markdown).toBe(markdown)
    expect(markdown?.content).toContain("Queue: empty")
  })

  test("native mouse scroll preserves identities while remaining inside the physical window", async () => {
    const { app, setup } = await fixture(Array.from({ length: 30 }, (_, index) => conversationItem(index + 1, "assistant", `Answer ${index}\nSecond line`)))
    const rows = new Map(app.transcript.mountedCards)
    const before = app.transcript.scroller.scrollTop
    await setup.mockMouse.scroll(app.transcript.scroller.x + 2, app.transcript.scroller.y + 2, "up")
    await setup.flush()
    expect(app.transcript.scroller.scrollTop).toBeLessThan(before)
    for (const [id, row] of app.transcript.mountedCards) expect(rows.get(id)).toBe(row)
  })

  test("clicking a retained answer leaves the composer writable", async () => {
    const { app, setup } = await fixture([conversationItem(1, "assistant", "A selectable answer")])
    const row = app.transcript.mountedCards.get("1")
    if (row === undefined) throw new Error("missing answer")
    await setup.mockMouse.click(row.markdown.x + 1, row.markdown.y)
    await setup.mockInput.typeText("next task")
    expect(app.composer.value).toBe("next task")
    expect(renderer?.currentFocusedRenderable).toBe(app.composer.editor)
  })

  test("the semantic turn footer labels subscription usage separately from context occupancy", async () => {
    const usage = { ...neverUsage(), input_tokens: "1200", output_tokens: "34" }
    const summary: TranscriptItem = {
      id: "3", ordinal: "2", revision: "3", agent_turn: "1", content: {
        type: "turn_summary", turn_id: "1", status: "completed", usage, cost: { kind: "subscription_quota", used: null, unit: null }
      }
    }
    const { app } = await fixture([conversationItem(1, "user", "What is the context?"), conversationItem(2, "assistant", "The answer"), summary], {
      ...createInitialState(), context: {
        turn_id: "1", stable_prefix_hash: "hash", used_tokens: "5000", usable_tokens: "100000",
        reserved_tokens: "0", context_window_known: true, cache_breakpoints: [], items: []
      }
    })
    expect(app.transcript.mountedCards.get("1")?.header.plainText).toBe("you")
    expect(app.transcript.mountedCards.get("3")?.header.plainText).toContain("turn usage · 1234 tokens")
    expect(app.statusLine.plainText).toContain("ctx 5%")
  })

  test("active permission mode appears beside agent mode without unknown-state noise", async () => {
    const { app, setup } = await fixture([], { ...createInitialState(), permissions: permissionState("auto-safe") })
    expect(app.statusLine.plainText).toContain("EXECUTE")
    expect(app.statusLine.plainText).toContain("auto-safe")
    app.setState({ ...app.state, permissions: null })
    await setup.flush()
    expect(app.statusLine.plainText).toContain("EXECUTE")
    expect(app.statusLine.plainText).not.toContain("auto-safe")
  })

  test("committed reasoning stays readable and collapses without taking composer focus", async () => {
    const { app, setup } = await fixture([conversationItem(1, "assistant", "Ready", "Inspecting workspace\nRead Cargo.toml next.")])
    const reasoning = app.transcript.mountedCards.get("1")?.reasoning
    if (reasoning === undefined) throw new Error("missing reasoning")
    expect(reasoning.expanded).toBe(true)
    expect(setup.captureCharFrame()).toContain("Read Cargo.toml next.")
    await setup.mockMouse.click(reasoning.header.x + 2, reasoning.header.y)
    await setup.flush()
    expect(reasoning.expanded).toBe(false)
    expect(reasoning.body.visible).toBe(false)
    expect(renderer?.currentFocusedRenderable).toBe(app.composer.editor)
  })

  test("live reasoning transfers its deliberate collapsed state into the committed source row", async () => {
    const items: TranscriptItem[] = []
    const initial = {
      ...createInitialState(), streamingTail: createStreamingTail({
        turnId: "1", text: "", thinking: "Reading manifests now.",
        citations: [], toolInvocationIds: [], finished: null
      })
    } satisfies RottweilerState
    const { app, setup } = await fixture(items, initial)
    const live = app.transcript.streamingCard.getChildren().find((child): child is ReasoningBlockRenderable => child instanceof ReasoningBlockRenderable)
    expect(live?.expanded).toBe(true)
    app.transcript.selectNextBlock()
    app.transcript.toggleSelectedBlock()
    expect(live?.expanded).toBe(false)
    items.push(conversationItem(1, "assistant", "", initial.streamingTail.thinking))
    app.transcript.scrollTo(0)
    await waitForHistory(setup, () => app.transcript.mountedCards.has("1"))
    app.setState({ ...initial, streamingTail: null })
    await setup.flush()
    const committed = app.transcript.mountedCards.get("1")?.reasoning
    expect(committed?.expanded).toBe(false)
    expect(app.transcript.selectedBlockId).toBe("history-reasoning:1")
  })

  test("completed tool content remains source-owned across workspace projection changes", async () => {
    const { app, setup } = await fixture([toolItem(1, "read", '{"path":"README.md"}', "Retained tool output")])
    const row = app.transcript.mountedCards.get("1")
    row?.toggle()
    app.setState({ ...app.state, workspaceRoots: { generation: "2", roots: [], effectiveFromTurn: "1" } })
    await setup.flush()
    expect(app.transcript.mountedCards.get("1")).toBe(row)
    expect(row?.markdown.content).toContain("Retained tool output")
    expect(row?.header.plainText).toContain("read · done")
  })
})
