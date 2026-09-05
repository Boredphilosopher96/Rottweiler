import { SelectRenderableEvents } from "@opentui/core"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, expect, test } from "bun:test"
import { createRottweilerApp } from "../../src/app"
import { PROTOCOL_VERSION, type EngineEvent, type TodoReadResult } from "../../src/protocol"
import { emptySessionReader, waitForHistory } from "../fixtures/history"

let renderer: TestRenderer | undefined
afterEach(() => { renderer?.destroy(); renderer = undefined })
const ack = (request = "attach") => ({ protocol_version: PROTOCOL_VERSION, client_id: "tasks", request_id: request, emitted_at: "2026-01-01T00:00:00Z" })
const ready: EngineEvent = { type: "session_history_ready", meta: ack(), session_id: "task-session", through_sequence: "20" }
const task = (content: string, through = "20"): TodoReadResult => ({ type: "ready", todos: {
  through, snapshot: { items: [{ id: "audit", content, status: "pending" }] },
} })

async function setup() {
  const fixture = await createTestRenderer({ width: 120, height: 30, useThread: false })
  renderer = fixture.renderer
  const reads: { session: string; signal: AbortSignal; resolve(result: TodoReadResult): void; reject(error: Error): void }[] = []
  const app = createRottweilerApp(renderer, {
    sessionReader: { ...emptySessionReader, todos: ({ sessionId: session }, signal) => new Promise((resolve, reject) => reads.push({ session, signal, resolve, reject })) },
    sessionId: "task-session", clientId: "tasks",
  })
  renderer.root.add(app)
  return { ...fixture, app, reads }
}

test("mounted task panel catches up with one owned query and retires retries on destroy", async () => {
  const fixture = await setup()
  const { app, reads, renderOnce, captureCharFrame } = fixture
  app.handleEvent(ready)
  expect(reads).toHaveLength(1)
  expect(reads[0]?.session).toBe("task-session")
  await renderOnce()
  expect(captureCharFrame()).toContain("Loading tasks")
  reads[0]!.resolve({ type: "catching_up", through: "10", target: "20" })
  await waitForHistory(fixture, () => reads.length === 2)
  reads[1]!.resolve(task("Inspect exact task state"))
  await waitForHistory(fixture, () => app.state.todos.phase === "ready")
  expect(captureCharFrame()).toContain("Inspect exact task state")
  expect(app.state.lastSequence).toBeNull()
  app.handleEvent({ type: "conversation_rewound", meta: { protocol_version: PROTOCOL_VERSION, session_id: "task-session", sequence_id: "21", emitted_at: "2026-01-01T00:00:00Z" }, to_agent_turn: "1", operation_id: "rewind", unrestorable_paths: [] })
  expect(app.state.todos.snapshot.items).toHaveLength(0)
  await waitForHistory(fixture, () => reads.length === 3)
  reads[2]!.resolve({ type: "catching_up", through: "20", target: "21" })
  await Bun.sleep(0)
  app.destroy()
  await Bun.sleep(70)
  expect(reads).toHaveLength(3)
})

test("superseded task reads cannot replace a newer live commit or another session", async () => {
  const fixture = await setup()
  const { app, reads } = fixture
  app.handleEvent(ready)
  app.handleEvent({ type: "todo_state_committed", meta: { protocol_version: PROTOCOL_VERSION, session_id: "task-session", sequence_id: "21", emitted_at: "2026-01-01T00:00:00Z" }, snapshot: { items: [{ id: "live", content: "New live state", status: "in_progress" }] } })
  reads[0]!.resolve(task("Stale query state"))
  await fixture.flush()
  expect(app.state.todos.snapshot.items[0]?.content).toBe("New live state")
  app.handleEvent(ready)
  const late = reads[1]!
  app.setSessionId("another-session")
  expect(late.signal.aborted).toBeTrue()
  late.resolve(task("Wrong session state", "999"))
  await fixture.flush()
  expect(app.state.todos.snapshot.items.some(item => item.content === "Wrong session state")).toBeFalse()
})

test("task read failures expose one explicit retry action", async () => {
  const fixture = await setup()
  const { app, reads, captureCharFrame } = fixture
  app.handleEvent(ready)
  reads[0]!.reject(new Error("Task index is busy"))
  await waitForHistory(fixture, () => app.state.todos.phase === "failed")
  expect(captureCharFrame()).toContain("Retry loading tasks")
  app.contextPanel.todos.emit(SelectRenderableEvents.ITEM_SELECTED, 0)
  expect(reads).toHaveLength(2)
  expect(app.state.todos.phase).toBe("loading")
  app.contextPanel.todos.emit(SelectRenderableEvents.ITEM_SELECTED, 0)
  expect(reads).toHaveLength(2)
})
