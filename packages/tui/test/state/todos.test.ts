import { expect, test } from "bun:test"
import { PROTOCOL_VERSION, type EngineEvent, type TodoReadResult } from "../../src/protocol"
import { createInitialState, MAX_RETAINED_TOOL_PROJECTIONS } from "../../src/state"
import { meta, reduce } from "./fixtures"

const snapshot = (id: string) => ({ items: [{ id, content: `Task ${id}`, status: "pending" as const }] })
function read(result: TodoReadResult): EngineEvent {
  return { type: "todos_read", meta: { protocol_version: PROTOCOL_VERSION, client_id: "client", request_id: "read", emitted_at: "2026-01-01T00:00:00Z" }, session_id: "session-state", result }
}

test("task state is committed independently of failed or transformed tool presentation", () => {
  let state = reduce(createInitialState(), { type: "todo_state_committed", meta: meta("1"), snapshot: snapshot("authoritative") })
  state = reduce(state, { type: "tool_call_started", meta: meta("2"), turn_id: "1", tool_call_id: "todo", invocation_id: "todo-1", name: "todo", args: { action: "replace" }, call_index: 0 })
  state = reduce(state, { type: "tool_call_finished", meta: meta("3"), turn_id: "1", tool_call_id: "todo", invocation_id: "todo-1", output: { type: "structured", value: snapshot("presentation-only") }, is_error: false, call_index: 0 })
  expect(state.todos.snapshot).toEqual(snapshot("authoritative"))
  state = reduce(state, { type: "tool_call_finished", meta: meta("4"), turn_id: "1", tool_call_id: "failed", invocation_id: "failed", output: { type: "text", text: "Output hook failed after the state commit" }, is_error: true, call_index: 1 })
  expect(state.todos.snapshot).toEqual(snapshot("authoritative"))
})

test("rewind waits for an exact current-prefix read instead of retaining tool checkpoints", () => {
  let state = reduce(createInitialState(), { type: "todo_state_committed", meta: meta("1"), snapshot: snapshot("later") })
  state = reduce(state, { type: "conversation_rewound", meta: meta("2"), to_agent_turn: "1", operation_id: "rewind", unrestorable_paths: [] })
  expect(state.todos.phase).toBe("loading")
  expect(state.todos.snapshot.items).toHaveLength(0)
  state = reduce(state, read({ type: "ready", todos: { through: "1", snapshot: snapshot("stale") } }))
  expect(state.todos.phase).toBe("loading")
  state = reduce(state, read({ type: "catching_up", through: "1", target: "2" }))
  expect(state.todos.snapshot.items).toHaveLength(0)
  state = reduce(state, read({ type: "ready", todos: { through: "2", snapshot: snapshot("restored") } }))
  expect(state.todos.snapshot).toEqual(snapshot("restored"))
  expect(state.lastSequence).toBe("2") // Connection-scoped reads never advance replay.
  state = reduce(state, { type: "todo_state_committed", meta: meta("3"), snapshot: snapshot("live") })
  state = reduce(state, read({ type: "ready", todos: { through: "2", snapshot: snapshot("obsolete") } }))
  state = reduce(state, read({ type: "catching_up", through: "1", target: "2" }))
  expect(state.todos.phase).toBe("ready")
  expect(state.todos.snapshot).toEqual(snapshot("live"))
})

test("task history has one snapshot and uses the same completed tool retention as every tool", () => {
  let state = createInitialState()
  let seq = 0
  for (let index = 0; index < 300; index++) {
    state = reduce(state, { type: "todo_state_committed", meta: meta(String(++seq)), snapshot: snapshot(String(index)) })
    state = reduce(state, { type: "tool_call_started", meta: meta(String(++seq)), turn_id: String(index), tool_call_id: `todo-${index}`, invocation_id: `todo-${index}`, name: "todo", args: null, call_index: 0 })
    state = reduce(state, { type: "tool_call_finished", meta: meta(String(++seq)), turn_id: String(index), tool_call_id: `todo-${index}`, invocation_id: `todo-${index}`, output: { type: "text", text: "done" }, is_error: false, call_index: 0 })
  }
  expect(Object.keys(state.tools)).toHaveLength(MAX_RETAINED_TOOL_PROJECTIONS)
  expect(state.todos.snapshot).toEqual(snapshot("299"))
  expect(JSON.stringify(state.todos).length).toBeLessThan(250)
})

test("generated task boundary rejects excess bytes, duplicate identity and undeclared count", async () => {
  const { isWireEngineEvent } = await import("../../src/transport")
  const event = (items: unknown[]) => ({ type: "todo_state_committed", meta: meta("1"), snapshot: { items } })
  const item = { id: "a", content: "é".repeat(2048), status: "pending" }
  expect(isWireEngineEvent(event([item]))).toBe(true)
  expect(isWireEngineEvent(event([{ ...item, content: `${item.content}x` }]))).toBe(false)
  expect(isWireEngineEvent(event([item, item]))).toBe(false)
  expect(isWireEngineEvent(event([{ ...item, content: "\u2003\t " }]))).toBe(false)
  expect(isWireEngineEvent(event([{ ...item, status: undefined }]))).toBe(false)
  expect(isWireEngineEvent(event(Array.from({ length: 129 }, (_, index) => ({ ...item, id: String(index), content: "a" }))))).toBe(false)
  expect(isWireEngineEvent(event(Array.from({ length: 17 }, (_, index) => ({ ...item, id: String(index) }))))).toBe(false)
  expect(isWireEngineEvent({ ...event([]), snapshot: { items: [], count: 0 } })).toBe(false)
  const query = read({ type: "ready", todos: { through: null, snapshot: { items: [] } } })
  expect(isWireEngineEvent(query)).toBe(true)
  expect(isWireEngineEvent({ ...query, result: { type: "ready", todos: { snapshot: { items: [] } } } })).toBe(false)
  expect(isWireEngineEvent({ ...query, result: { type: "catching_up", through: null } })).toBe(false)
})
