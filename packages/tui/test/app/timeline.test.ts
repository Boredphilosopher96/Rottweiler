import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import { createRottweilerApp } from "../../src/app"
import type { ClientCommand, EngineEvent, TranscriptItem } from "../../src/protocol"
import type { SessionReader } from "../../src/session-reader"
import { createInitialState } from "../../src/state"
import { conversationItem, emptySessionReader, sessionReaderFor, waitForHistory } from "../fixtures/history"

function history(text: string, attachments = false, source = 10): SessionReader {
  const item = conversationItem(source, "user", text.slice(0, 50))
  if (item.content.type === "conversation") {
    item.content.omitted_blocks = attachments
    const block = item.content.blocks[0]
    if (block?.type === "text") block.body.complete = text.length <= 50
  }
  return { ...sessionReaderFor([item]), content: async (_session, read) => {
    const textBytes = Buffer.from(text)
    const chunk = textBytes.subarray(read.offset, read.offset + read.max_bytes).toString()
    const end = read.offset + Buffer.byteLength(chunk)
    return { view: read.view, source: read.source, text: chunk, offset: read.offset, format: "text",
      total_bytes: textBytes.length, next_offset: end === textBytes.length ? null : end }
  } }
}
function rewindEvent(request: string | undefined, sequence = "1"): EngineEvent {
  return { type: "conversation_rewound", meta: { protocol_version: PROTOCOL_VERSION,
    session_id: "session-local", sequence_id: sequence, emitted_at: "2026-01-01T00:00:00Z",
    ...(request === undefined ? {} : { caused_by: request }) },
    to_agent_turn: "9", operation_id: "operation", unrestorable_paths: [] }
}

describe("Rottweiler semantic timeline", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => { renderer?.destroy(); renderer = undefined })

  async function setup(reader: SessionReader, options: Partial<Parameters<typeof createRottweilerApp>[1]> = {}) {
    const testRenderer = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = testRenderer.renderer
    const commands: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
      sessionReader: reader, requestId: () => `timeline-${request++}`,
      onCommand(command) { commands.push(command); return { type: "accepted" } }, ...options,
    })
    renderer.root.add(app)
    return { app, commands, ...testRenderer }
  }
  async function selectAction(result: Awaited<ReturnType<typeof setup>>, action: "edit" | "retry" | "rewind") {
    result.app.openTimelinePicker()
    await waitForHistory(result, () => result.app.picker.select.options.some(option => option.value === "timeline.turn.10"))
    result.app.picker.select.selectCurrent()
    const index = result.app.picker.select.options.findIndex(option => option.value === `timeline.action.${action}`)
    result.app.picker.select.setSelectedIndex(index)
    result.app.picker.select.selectCurrent()
    await Bun.sleep(0)
  }

  test("/rewind opens asynchronous semantic history without submitting a message", async () => {
    const result = await setup(emptySessionReader, { initialState: {
      ...createInitialState(), commands: [{ name: "rewind", description: "Rewind the conversation", usage: "/rewind" }],
    } })
    await result.mockInput.typeText("/rew")
    result.mockInput.pressEnter()
    await waitForHistory(result, () => result.app.picker.status.plainText.includes("No user turns"))
    expect(result.app.composer.value).toBe("")
    expect(result.app.picker.title).toContain("Conversation timeline")
    expect(result.commands.some(command => command.type === "send_message")).toBe(false)
  })

  test("newest-first pages restore old user sources independently of recent raw events", async () => {
    const items: TranscriptItem[] = Array.from({ length: 400 }, (_, index) => conversationItem(index, "user", `Request ${index}`))
    const result = await setup(sessionReaderFor(items))
    expect("transcript" in result.app.state).toBe(false)
    result.app.openTimelinePicker()
    await waitForHistory(result, () => result.app.picker.select.options.some(option => option.value === "timeline.turn.399"))
    expect(result.app.picker.select.options.filter(option => String(option.value).startsWith("timeline.turn.")).map(option => option.name))
      .toEqual(Array.from({ length: 32 }, (_, index) => `Request ${399 - index}`))
    for (let page = 0; page < 10; page++) {
      const older = result.app.picker.select.options.findIndex(option => option.value === "timeline.older")
      result.app.picker.select.setSelectedIndex(older)
      result.app.picker.select.selectCurrent()
      await Bun.sleep(0)
    }
    expect(result.app.picker.select.options.some(option => option.value === "timeline.turn.79")).toBe(true)
  })

  test("renderer handoff restores the selected historical timeline source", async () => {
    const items = Array.from({ length: 400 }, (_, index) => conversationItem(index, "user", `Request ${index}`))
    const reader = sessionReaderFor(items)
    const result = await setup(reader)
    result.app.openTimelinePicker()
    await waitForHistory(result, () => result.app.picker.select.options.some(option => option.value === "timeline.turn.399"))
    for (let page = 0; page < 10; page++) {
      const older = result.app.picker.select.options.findIndex(option => option.value === "timeline.older")
      result.app.picker.select.setSelectedIndex(older)
      result.app.picker.select.selectCurrent()
      await Bun.sleep(0)
    }
    expect(result.app.recycleState()).toBeNull()
    const selected = result.app.picker.select.options.findIndex(option => option.value === "timeline.turn.70")
    result.app.picker.select.setSelectedIndex(selected)
    const state = result.app.recycleState()
    expect(state).not.toBeNull()
    result.app.destroyRecursively()
    const restored = createRottweilerApp(result.renderer, { sessionReader: reader })
    result.renderer.root.add(restored)
    restored.restoreRecycleState(state!)
    await waitForHistory(result, () => {
      restored.applyPendingRecycleScroll()
      return restored.picker.select.getSelectedOption()?.value === "timeline.turn.70"
    })
    expect(restored.picker.title).toContain("Conversation timeline")
    expect(restored.picker.select.getSelectedOption()?.value).toBe("timeline.turn.70")
  })

  test("edit reads exact content, sends a source precondition and preserves concurrent composer text", async () => {
    const original = "Keep this exact text\n" + "including the full source body. ".repeat(300)
    const result = await setup(history(original, true))
    await selectAction(result, "edit")
    const command = result.commands.find(command => command.type === "rewind")!
    expect(command).toMatchObject({ type: "rewind", session_id: "session-local",
      target: { type: "source", source: "10", expected_through: "10", turn_id: "10", position: "before" } })
    expect(result.commands.some(command => command.type === "send_message")).toBe(false)
    await result.mockInput.typeText("new draft during rewind")
    result.app.handleEvent(rewindEvent(undefined))
    expect(result.app.composer.value).toBe("new draft during rewind")
    result.app.handleEvent(rewindEvent(command.meta.request_id, "2"))
    expect(result.app.composer.value).toBe(`${original}\nnew draft during rewind`)
    expect(result.renderer.currentFocusedRenderable).toBe(result.app.composer.editor)
    expect(result.app.banner.plainText).toContain("attachments from the original message are not restored")
  })

  test("retry sends exact source text only after matching durable causation", async () => {
    const original = "  preserve whitespace\nand newlines exactly  "
    const result = await setup(history(original))
    await selectAction(result, "retry")
    const command = result.commands.find(command => command.type === "rewind")!
    expect(result.commands.filter(command => command.type === "send_message")).toHaveLength(0)
    result.app.handleEvent(rewindEvent("unrelated-request"))
    await Bun.sleep(0)
    expect(result.commands.filter(command => command.type === "send_message")).toHaveLength(0)
    result.app.handleEvent(rewindEvent(command.meta.request_id, "2"))
    await Bun.sleep(0)
    expect(result.commands.filter(command => command.type === "send_message"))
      .toEqual([expect.objectContaining({ content: original, attachments: [] })])
    result.app.handleEvent(rewindEvent(command.meta.request_id, "3"))
    await Bun.sleep(0)
    expect(result.commands.filter(command => command.type === "send_message")).toHaveLength(1)
  })

  test("a rejected source mutation cannot restore its draft on a late rewind event", async () => {
    const result = await setup(history("Do not restore"), { onCommand: command => command.type === "rewind"
      ? { type: "rejected", error: { category: "protocol", code: "stale_source", message: "History changed", retryable: true } }
      : { type: "accepted" } })
    await selectAction(result, "edit")
    expect(result.app.state.errors.at(-1)?.code).toBe("stale_source")
    result.app.handleEvent(rewindEvent("timeline-0"))
    expect(result.app.composer.value).toBe("")
  })

  test("history exceeding the interactive text limit refuses before rewind dispatch and preserves the draft", async () => {
    const reader = history("x")
    const result = await setup({ ...reader, content: async (session, read, signal, allocation) => ({
      ...await reader.content(session, read, signal, allocation), text: "x".repeat(4096), total_bytes: 16 * 1024 * 1024, next_offset: 4096,
    }) })
    result.app.composer.value = "keep this draft"
    await selectAction(result, "edit")
    expect(result.commands.some(command => command.type === "rewind")).toBe(false)
    expect(result.app.composer.value).toBe("keep this draft")
    expect(result.app.state.errors.at(-1)?.message).toContain("Attach large content as a file")
  })

  test("rewind-only uses the selected completed boundary without loading its body", async () => {
    const result = await setup({ ...history("body"), content: async () => { throw new Error("must not load") } })
    await selectAction(result, "rewind")
    expect(result.commands.find(command => command.type === "rewind")).toMatchObject({
      target: { type: "source", source: "10", position: "through" },
    })
  })

  test("replay timelines remain browsable without mutation actions", async () => {
    const result = await setup(history("Historical request"), { replaySessionId: "historical-timeline" })
    result.app.openTimelinePicker()
    await waitForHistory(result, () => result.app.picker.select.options.some(option => option.name === "Historical request"))
    expect(result.app.picker.select.options.map(option => option.name)).toEqual(["read-only session", "Historical request"])
    result.app.picker.select.selectCurrent()
    expect(result.app.picker.title).toContain("Conversation timeline")
    expect(result.commands.filter(command => command.type === "rewind")).toHaveLength(0)
  })
})
