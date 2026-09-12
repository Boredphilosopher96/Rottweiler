import { expect, test } from "bun:test"
import { createTestRenderer } from "@opentui/core/testing"
import type { Renderable } from "@opentui/core"
import { createRottweilerApp, type RottweilerApp } from "../../src/app"
import { PROTOCOL_VERSION, type ClientCommand } from "../../src/protocol"
import { createInitialState } from "../../src/state"
import { emptySessionReader, waitForHistory } from "../fixtures/history"

function focused(root: Renderable): Renderable[] {
  const result: Renderable[] = []
  function visit(node: Renderable) { if (node.focused) result.push(node); for (const child of node.getChildren()) visit(child) }
  visit(root)
  return result
}

test.each(["approval", "question", "text question"] as const)("overlay, %s, session switch, late query and Escape retain one focus and the unsent draft", async interaction => {
  const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
  const started = Promise.withResolvers<void>(), release = Promise.withResolvers<void>(), delivered = Promise.withResolvers<void>()
  const commands: ClientCommand[] = []
  let app!: RottweilerApp
  app = createRottweilerApp(setup.renderer, { sessionId: "origin", clientId: "c", sessionReader: emptySessionReader,
    initialState: { ...createInitialState(), driverClientId: "c" },
    onSessionSelect: id => { app.setState({ ...createInitialState(), driverClientId: "c" }); app.setSessionId(id); app.setState(app.state) },
    async onCommand(command) {
      commands.push(command)
      const meta = { ...command.meta, emitted_at: "2026-09-08T00:00:00Z" }
      if (command.type === "list_sessions") {
        await Bun.sleep(0)
        app.handleEvent({ type: "sessions_listed", meta, sessions: [] })
      }
      if (command.type === "search_sessions") {
        started.resolve(); await release.promise
        app.handleEvent({ type: "sessions_search_ready", meta, query: command.query, truncated: false,
          hits: [{ session: { session_id: "obsolete", title: "Old query result", workspace_name: "Old workspace", model: "fast", driver_client_id: null, shell_active: false }, match: null }] })
        delivered.resolve()
      }
      return { type: "accepted" }
    },
  })
  setup.renderer.root.add(app)
  const oneFocus = (expected: Renderable) => {
    expect(focused(app).map(node => node.id)).toEqual([expected.id]); expect(setup.renderer.currentFocusedRenderable?.id).toBe(expected.id)
    expect(app.composer.value).toBe("unsent draft 界")
  }
  try {
    app.composer.value = "unsent draft 界"
    await setup.flush(); oneFocus(app.composer.editor)
    app.openSessionPicker(); await waitForHistory(setup, () => app.picker.input.focused); oneFocus(app.picker.input)
    if (interaction === "approval") app.handleEvent({ type: "tool_approval_needed", meta: { protocol_version: PROTOCOL_VERSION, session_id: "origin", sequence_id: "1", emitted_at: "2026-09-08T00:00:00Z" },
      turn_id: "turn", tool_call_id: "call", invocation_id: "invocation", name: "write", args: { path: "file.txt" },
      capabilities: ["write_filesystem"], rationale: "Approve write", diff: null })
    else app.handleEvent({ type: "question_asked", meta: { protocol_version: PROTOCOL_VERSION, session_id: "origin", sequence_id: "1", emitted_at: "2026-09-08T00:00:00Z" },
      turn_id: "turn", question_id: "q", question: { id: "q", prompt: "Choose", response_kind: interaction === "text question" ? "text" : "select_one", options: [
        { label: "First", value: "first", description: null }, { label: "Second", value: "second", description: null },
      ] },
    })
    await setup.flush(); oneFocus(app.picker.input)
    expect(app.interactionPanel.visible).toBeTrue()
    if (interaction === "approval") expect(app.state.tools.invocation?.status).toBe("awaiting_approval")
    await setup.mockInput.typeText("filter")
    expect(app.picker.input.value).toBe("filter"); oneFocus(app.picker.input)
    await started.promise
    app.handleEvent({ type: "session_navigation_requested", session_id: "origin", target: { kind: "session", session_id: "next" },
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "c", request_id: "navigate", emitted_at: "2026-09-08T00:00:00Z" } })
    await waitForHistory(setup, () => !app.picker.visible)
    oneFocus(app.composer.editor)
    expect(app.state.tools.invocation).toBeUndefined()
    release.resolve(); await delivered.promise; await setup.flush()
    expect(app.state.sessions).toEqual([])
    expect(app.state.sessionSearch).toBeNull()
    expect(app.picker.visible).toBeFalse(); oneFocus(app.composer.editor)
    setup.mockInput.pressEscape(); await setup.flush(); oneFocus(app.composer.editor)
    expect(commands.some(command => command.type === "approve_tool" || command.type === "answer_question" || command.type === "send_message")).toBeFalse()
  } finally { release.resolve(); app.destroy(); setup.renderer.destroy(); await Bun.sleep(0) }
})
