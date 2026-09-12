import { expect, test } from "bun:test"
import { createTestRenderer, MockTreeSitterClient } from "@opentui/core/testing"
import { createRottweilerApp, type RottweilerApp } from "../../src/app"
import { createInitialState } from "../../src/state"
import type { ClientCommand, SessionSearchMatch, TranscriptRead } from "../../src/protocol"
import { fixturePage, emptySessionReader, waitForHistory } from "../fixtures/history"

const source: SessionSearchMatch = { session_id: "matched", source_sequence: "900", through: "2000", digest: Array(32).fill(0) as SessionSearchMatch["digest"] }

async function fixture(match: SessionSearchMatch | null, failure = false) {
  const setup = await createTestRenderer({ width: 100, height: 30, useThread: false })
  const reads: { session: string; read: TranscriptRead }[] = []
  const switches: string[] = []
  const searchStarted = Promise.withResolvers<void>()
  const releaseSearch = Promise.withResolvers<void>()
  let app!: RottweilerApp
  app = createRottweilerApp(setup.renderer, {
    sessionId: "origin", treeSitterClient: new MockTreeSitterClient(),
    sessionReader: { ...emptySessionReader, page: async (target, read) => {
      reads.push({ session: target.sessionId, read })
      if (read.position.type === "search_match") {
        if (failure) throw new Error("Search source was removed")
        // Semantic identity differs from both the provider source and ordinal.
        const page = fixturePage(target.sessionId, { ...read, position: { type: "around", item: "42" } })
        return { type: "ready", page: { ...page, anchor: { type: "exact", item: "42" } } }
      }
      return { type: "ready", page: fixturePage(target.sessionId, read) }
    } },
    async onSessionSelect(id) {
      switches.push(id)
      app.setState(createInitialState()); app.setSessionId(id); app.setState(app.state)
    },
    async onCommand(command: ClientCommand) {
      await Bun.sleep(0)
      const meta = { ...command.meta, emitted_at: "2026-09-08T00:00:00Z" }
      if (command.type === "list_sessions") app.handleEvent({ type: "sessions_listed", meta, sessions: [] })
      if (command.type === "search_sessions") { searchStarted.resolve(); await releaseSearch.promise }
      if (command.type === "search_sessions") app.handleEvent({ type: "sessions_search_ready", meta, query: command.query, truncated: false,
        hits: [{ session: { session_id: "matched", title: "Different title", workspace_name: "Workspace", model: "fast", driver_client_id: null, shell_active: false }, match }] })
      return { type: "accepted" }
    },
  })
  setup.renderer.root.add(app)
  await waitForHistory(setup, () => app.transcript.mountedEntryCount > 0)
  app.composer.value = "unfinished draft"
  app.openSessionPicker()
  await waitForHistory(setup, () => app.picker.select.options.some(option => option.value === "sessions.new"))
  expect(setup.renderer.currentFocusedRenderable).toBe(app.picker.input)
  await setup.mockInput.typeText("body-only-needle")
  await searchStarted.promise
  try {
    // A state update while the remote search is pending must not reset its
    // editable query merely because the previous session list was empty.
    app.setState({ ...app.state })
    await setup.renderOnce()
    expect(app.picker.input.value).toBe("body-only-needle")
    expect(app.picker.input.visible).toBeTrue()
    expect(setup.renderer.currentFocusedRenderable).toBe(app.picker.input)
  } catch (error) {
    app.destroy(); setup.renderer.destroy(); throw error
  } finally { releaseSearch.resolve() }
  await waitForHistory(setup, () => app.picker.select.options.some(option => option.value === "matched"))
  app.picker.select.setSelectedIndex(app.picker.select.options.findIndex(option => option.value === "matched"))
  app.picker.select.selectCurrent(); await setup.flush()
  return { setup, app, reads, switches }
}

test("body-only session hit stays visible and opens its exact semantic row after normal session activation", async () => {
  const { setup, app, reads, switches } = await fixture(source)
  try {
    expect(app.picker.select.options.map(option => option.value)).toEqual(["match", "resume", "rename"])
    app.picker.select.selectCurrent()
    await waitForHistory(setup, () => app.transcript.captureHistoryViewport()?.anchor?.id === "42")
    expect(switches).toEqual(["matched"])
    expect(reads.find(item => item.read.position.type === "search_match")).toMatchObject({ session: "matched", read: { position: { type: "search_match", source } } })
    expect(app.transcript.mountedCards.has("42")).toBeTrue()
    expect(app.transcript.mountedCards.has("900")).toBeFalse()
    expect(app.composer.value).toBe("unfinished draft")
  } finally { app.destroy(); setup.renderer.destroy() }
})

test("title-only search retains session actions without inventing a transcript anchor", async () => {
  const { setup, app, reads } = await fixture(null)
  try {
    expect(app.picker.select.options.map(option => option.value)).toEqual(["resume", "rename"])
    expect(reads.some(item => item.read.position.type === "search_match")).toBeFalse()
  } finally { app.destroy(); setup.renderer.destroy() }
})

test("a removed search source reports failure and cannot silently choose another message", async () => {
  const { setup, app, reads } = await fixture(source, true)
  try {
    app.picker.select.selectCurrent()
    await waitForHistory(setup, () => app.state.errors.some(error => error.code === "search_navigation_failed"))
    expect(app.state.errors.at(-1)?.message).toBe("Search source was removed")
    expect(reads.filter(item => item.read.position.type === "search_match")).toHaveLength(1)
    expect(reads.some(item => item.read.position.type === "around")).toBeFalse()
    expect(app.composer.value).toBe("unfinished draft")
  } finally { app.destroy(); setup.renderer.destroy() }
})
