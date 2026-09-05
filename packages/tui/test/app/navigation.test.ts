import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, expect, test } from "bun:test"
import { createRottweilerApp } from "../../src/app"
import { PROTOCOL_VERSION, type EngineEvent, type TranscriptRead } from "../../src/protocol"
import { createInitialState } from "../../src/state"
import { conversationItem, sessionReaderFor, waitForHistory } from "../fixtures/history"

let renderer: TestRenderer | undefined
afterEach(() => { renderer?.destroy(); renderer = undefined })

function navigation(target: Extract<EngineEvent, { type: "session_navigation_requested" }>["target"], session = "session-tui-test", client = "driver"): EngineEvent {
  return { type: "session_navigation_requested", session_id: session, target,
    meta: { protocol_version: PROTOCOL_VERSION, client_id: client, request_id: "goto", emitted_at: "2026-01-01T00:00:00Z" } }
}

async function fixture(onAround?: () => Promise<void>) {
  const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
  renderer = setup.renderer
  const reader = sessionReaderFor(Array.from({ length: 1000 }, (_, i) => conversationItem(i + 1, "user", `Message ${i + 1}`)))
  const reads: TranscriptRead[] = []
  const selected: string[] = []
  const app = createRottweilerApp(renderer, {
    sessionId: "session-tui-test",
    initialState: { ...createInitialState(), driverClientId: "driver" },
    sessionReader: { ...reader, page: async (target, read, signal, allocation) => {
      reads.push(read)
      if (read.position.type === "around") await onAround?.()
      const result = await reader.page(target, read, signal, allocation)
      if (read.position.type === "around" && read.position.item === "0" && result.type === "ready") {
        return { ...result, page: { ...result.page, anchor: { type: "replaced", requested: "0", replacement: "1" } } }
      }
      return result
    } },
    onSessionSelect: id => { selected.push(id) },
  })
  renderer.root.add(app)
  await waitForHistory(setup, () => app.transcript.mountedEntryCount > 0)
  return { setup, app, reads, selected }
}

test("connection navigation reveals a bounded semantic window and preserves the draft and cursor", async () => {
  const { setup, app, reads } = await fixture()
  app.composer.value = "unfinished draft"
  const cursor = app.state.lastSequence
  app.handleEvent(navigation({ kind: "transcript", sequence: "500" }))
  await waitForHistory(setup, () => app.transcript.mountedCards.has("500"))
  expect(reads.some(read => read.position.type === "around" && read.position.item === "500")).toBeTrue()
  expect(app.transcript.mountedEntryCount).toBeLessThanOrEqual(16)
  expect(app.composer.value).toBe("unfinished draft")
  expect(app.state.lastSequence).toBe(cursor)
  expect(renderer?.currentFocusedRenderable).toBe(app.composer.editor)
})

test("reports a removed canonical anchor instead of pretending the requested row exists", async () => {
  const { setup, app } = await fixture()
  app.handleEvent(navigation({ kind: "transcript", sequence: "0" }))
  await waitForHistory(setup, () => app.banner.plainText.includes("showing item 1"))
  expect(app.banner.plainText).toContain("Transcript item 0 is unavailable; showing item 1.")
  expect(app.transcript.mountedCards.has("0")).toBeFalse()
})

test("session navigation uses the normal session selection owner and ignores foreign authority", async () => {
  const { setup, app, reads, selected } = await fixture()
  const readCount = reads.length
  app.handleEvent(navigation({ kind: "session", session_id: "next" }, "foreign"))
  app.handleEvent(navigation({ kind: "session", session_id: "next" }, "session-tui-test", "observer"))
  expect(selected).toEqual([])
  app.handleEvent(navigation({ kind: "session", session_id: "next" }))
  await waitForHistory(setup, () => selected.length > 0)
  expect(selected).toEqual(["next"])
  expect(reads.length).toBe(readCount)
})


test("owns one pending navigation and releases failed read state without losing a draft", async () => {
  let fail: ((error: Error) => void) | undefined
  const { setup, app, reads } = await fixture(() => new Promise<void>((_, reject) => { fail = reject }))
  app.composer.value = "preserve me"
  app.handleEvent(navigation({ kind: "transcript", sequence: "500" }))
  await waitForHistory(setup, () => fail !== undefined)
  expect(app.recycleState()).toBeNull()
  app.handleEvent(navigation({ kind: "transcript", sequence: "600" }))
  await waitForHistory(setup, () => app.state.errors.some(error => error.code === "navigation_pending"))
  expect(reads.filter(read => read.position.type === "around")).toHaveLength(1)
  fail?.(new Error("bounded history read failed"))
  await waitForHistory(setup, () => app.state.errors.some(error => error.code === "session_navigation_failed"))
  expect(app.composer.value).toBe("preserve me")
  expect(app.transcript.captureHistoryViewport()).not.toBeNull()
})

test("a historical replay cannot trigger a live session transition", async () => {
  const { setup, app, selected } = await fixture()
  app.setState({ ...app.state, replay: { ...app.state.replay, active: true } })
  app.handleEvent(navigation({ kind: "session", session_id: "next" }))
  await setup.flush()
  expect(selected).toEqual([])
})

test("source navigation retains the active question's keyboard ownership", async () => {
  const { setup, app } = await fixture()
  app.setState({ ...app.state, questions: { choice: { questionId: "choice", turnId: "2", questions: [{
    id: "choice", prompt: "Choose", response_kind: "select_one",
    options: [{ value: "keep", label: "Keep", description: "Keep the change" }],
  }] } } })
  await setup.flush()
  const focused = renderer?.currentFocusedRenderable
  expect(focused).toBe(app.interactionPanel.select)
  app.handleEvent(navigation({ kind: "transcript", sequence: "500" }))
  await waitForHistory(setup, () => app.transcript.mountedCards.has("500"))
  expect(renderer?.currentFocusedRenderable).toBe(focused)
  expect(app.state.questions.choice).toBeDefined()
})
