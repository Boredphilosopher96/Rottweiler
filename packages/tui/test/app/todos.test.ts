import { SelectRenderableEvents } from "@opentui/core"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, expect, test } from "bun:test"
import { createRottweilerApp } from "../../src/app"
import { PROTOCOL_VERSION, type ClientCommand, type EngineEvent } from "../../src/protocol"
import { emptyHistoryReader } from "../fixtures/history"

let renderer: TestRenderer | undefined
afterEach(() => { renderer?.destroy(); renderer = undefined })
const ack = (request = "attach") => ({ protocol_version: PROTOCOL_VERSION, client_id: "tasks", request_id: request, emitted_at: "2026-01-01T00:00:00Z" })
const ready: EngineEvent = { type: "session_history_ready", meta: ack(), session_id: "task-session", through_sequence: "20" }

async function setup(failTasks = false) {
  const fixture = await createTestRenderer({ width: 120, height: 30, useThread: false })
  renderer = fixture.renderer
  const commands: ClientCommand[] = []
  const app = createRottweilerApp(renderer, {
    historyReader: emptyHistoryReader, sessionId: "task-session", clientId: "tasks",
    requestId: () => `request-${commands.length + 1}`,
    onCommand(command) {
      commands.push(command)
      if (command.type === "get_todos" && failTasks) return { type: "rejected", error: {
        category: "protocol", code: "tasks_unavailable", message: "Task index is busy", retryable: true,
      } }
      return { type: "accepted" }
    },
  })
  renderer.root.add(app)
  return { ...fixture, app, commands, setTaskFailure(value: boolean) { failTasks = value }, reads: () => commands.filter((command) => command.type === "get_todos") }
}

test("mounted task panel catches up with one owned query and retires retries on destroy", async () => {
  const { app, reads, renderOnce, captureCharFrame } = await setup()
  app.handleEvent(ready)
  expect(reads()).toHaveLength(1)
  await renderOnce()
  expect(captureCharFrame()).toContain("Loading tasks")
  app.handleEvent({ type: "todos_read", meta: ack(reads()[0]!.meta.request_id), session_id: "task-session", result: { type: "catching_up", through: "10", target: "20" } })
  expect(reads()).toHaveLength(1)
  await Bun.sleep(70)
  expect(reads()).toHaveLength(2)
  app.handleEvent({ type: "todos_read", meta: ack(reads()[0]!.meta.request_id), session_id: "task-session", result: { type: "ready", todos: { through: "20", snapshot: { items: [] } } } })
  expect(app.state.todos.phase).toBe("loading")
  app.handleEvent({ type: "todos_read", meta: ack(reads()[1]!.meta.request_id), session_id: "task-session", result: { type: "ready", todos: { through: "20", snapshot: { items: [{ id: "audit", content: "Inspect exact task state", status: "pending" }] } } } })
  await renderOnce()
  expect(captureCharFrame()).toContain("Inspect exact task state")
  expect(app.state.lastSequence).toBeNull()
  app.handleEvent({ type: "conversation_rewound", meta: { protocol_version: PROTOCOL_VERSION, session_id: "task-session", sequence_id: "21", emitted_at: "2026-01-01T00:00:00Z" }, to_agent_turn: "1", operation_id: "rewind", unrestorable_paths: [] })
  expect(app.state.todos.snapshot.items).toHaveLength(0)
  expect(reads()).toHaveLength(3)
  app.handleEvent({ type: "todos_read", meta: ack(reads()[2]!.meta.request_id), session_id: "task-session", result: { type: "catching_up", through: "20", target: "21" } })
  app.destroy()
  await Bun.sleep(70)
  expect(reads()).toHaveLength(3)
})

test("task replies are scoped to their authenticated session", async () => {
  const { app, reads } = await setup()
  app.handleEvent(ready)
  const request = reads()[0]!.meta.request_id
  app.handleEvent({ type: "todos_read", meta: ack(request), session_id: "other-session", result: { type: "ready", todos: { through: "20", snapshot: { items: [] } } } })
  expect(app.state.todos.phase).toBe("loading")
  expect(reads()).toHaveLength(1)
})

test("task read failures expose a bounded explicit retry action", async () => {
  const { app, reads, renderOnce, captureCharFrame, setTaskFailure } = await setup(true)
  app.handleEvent(ready)
  await Bun.sleep(0)
  await renderOnce()
  expect(app.state.todos.phase).toBe("failed")
  expect(captureCharFrame()).toContain("Retry loading tasks")
  setTaskFailure(false)
  app.contextPanel.todos.emit(SelectRenderableEvents.ITEM_SELECTED, 0)
  expect(reads()).toHaveLength(2)
  expect(app.state.todos.phase).toBe("loading")
  app.contextPanel.todos.emit(SelectRenderableEvents.ITEM_SELECTED, 0)
  expect(reads()).toHaveLength(2)
})
