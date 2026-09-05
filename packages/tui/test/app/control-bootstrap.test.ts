import { createTestRenderer } from "@opentui/core/testing"
import { expect, test } from "bun:test"
import { createRottweilerApp } from "../../src/app"
import { PROTOCOL_VERSION, type EngineEvent } from "../../src/protocol"
import { emptySessionReader, waitForHistory } from "../fixtures/history"

test("a recreated renderer restores draft state and reconciles a fresh bounded interaction snapshot", async () => {
  const first = await createTestRenderer({ width: 80, height: 18, useThread: false })
  const original = createRottweilerApp(first.renderer, { sessionId: "s", sessionReader: emptySessionReader })
  first.renderer.root.add(original)
  original.composer.value = "unfinished user draft"
  await first.flush()
  const saved = original.recycleState()
  expect(saved).not.toBeNull()
  first.renderer.destroy()
  const setup = await createTestRenderer({ width: 70, height: 20, useThread: false })
  try {
    const app = createRottweilerApp(setup.renderer, { sessionId: "s", sessionReader: emptySessionReader })
    setup.renderer.root.add(app)
    app.restoreRecycleState(saved!)
    const event: EngineEvent = {
      type: "session_controls_ready", session_id: "s",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "client", request_id: "controls", emitted_at: "2026-01-01T00:00:00Z" },
      snapshot: { through: "25", controls: { approvals: [], pending_plan: null, questions: [{
        question_id: "choice", turn_id: "2", questions: [{ id: "choice", prompt: "Choose", response_kind: "select_one",
          options: [{ value: "keep", label: "Keep", description: "Keep this change" }] }],
      }] } },
    }
    app.handleEvent(event)
    await waitForHistory(setup, () => setup.renderer.currentFocusedRenderable === app.interactionPanel.select)
    expect(app.composer.value).toBe("unfinished user draft")
    expect(app.state.lastSequence).toBeNull()
    expect(app.state.questions.choice).toBeDefined()
    app.handleEvent({ type: "question_answered", meta: { protocol_version: PROTOCOL_VERSION, session_id: "s", sequence_id: "26", emitted_at: "2026-01-01T00:00:01Z" }, turn_id: "2", question_id: "choice", answers: [] })
    await waitForHistory(setup, () => setup.renderer.currentFocusedRenderable === app.composer.editor)
    expect(app.state.questions.choice).toBeUndefined()
    expect(app.composer.value).toBe("unfinished user draft")
  } finally { setup.renderer.destroy() }
})
