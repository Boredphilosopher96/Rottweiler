import { expect, test } from "bun:test"
import { MAX_ACTIVE_CHILDREN, type SessionChildrenSnapshot } from "../../../protocol/types"
import { PROTOCOL_VERSION, type EngineEvent } from "../src/protocol"
import { createInitialState, engineEvent, reduceRottweilerState } from "../src/state"
const apply = (state: ReturnType<typeof createInitialState>, event: EngineEvent) => reduceRottweilerState(state, engineEvent(event), "s")
const meta = (sequence_id: string) => ({ protocol_version: PROTOCOL_VERSION, session_id: "s", sequence_id, emitted_at: "2026-01-01T00:00:00Z" })
const child = { subagent_id: "worker", child_session_id: "child", spawned: "3", spawned_turn: "1", task_preview: "Inspect source", task_truncated: false }
function ready(snapshot: SessionChildrenSnapshot): EngineEvent {
  return { type: "session_children_ready", session_id: "s", result: { type: "ready", snapshot },
    meta: { protocol_version: PROTOCOL_VERSION, client_id: "c", request_id: "r", emitted_at: "2026-01-01T00:00:00Z" } }
}
test("active child recovery rejects older lifecycle snapshots while replay cursor advances independently", () => {
  let state = apply(createInitialState(), ready({ through: "4", children: [child] }))
  expect(state.lastSequence).toBeNull()
  expect(state.subagents.worker).toMatchObject({ childSessionId: "child", status: "running", parentTurnId: "1" })
  state = apply(state, { type: "subagent_spawned", meta: meta("3"), subagent_id: "worker", child_session_id: "old-child", task: "old" })
  expect(state.subagents.worker?.childSessionId).toBe("child")
  expect(state.lastSequence).toBe("3")
  state = apply(state, { type: "conversation_rewound", meta: meta("4"), to_agent_turn: "0", operation_id: "rewind", unrestorable_paths: [] })
  expect(state.subagents.worker?.childSessionId).toBe("child")
  state = apply(state, { type: "subagent_spawned", meta: meta("5"), subagent_id: "new", child_session_id: "child-new", task: "New work" })
  expect(apply(state, ready({ through: "4", children: [child] }))).toBe(state)
  state = apply(state, ready({ through: "5", children: [] }))
  expect(state.subagents).toEqual({})
})
test("active child snapshot admission rejects duplicate, foreign source and excess associations", () => {
  const state = createInitialState()
  expect(() => apply(state, ready({ through: "3", children: [child, child] }))).toThrow("identity")
  expect(() => apply(state, ready({ through: "2", children: [child] }))).toThrow("source")
  expect(() => apply(state, ready({ through: "3", children: Array.from({length: MAX_ACTIVE_CHILDREN + 1}, (_, i) => ({ ...child, subagent_id: String(i) })) }))).toThrow("allocation")
})
