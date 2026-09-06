import { ClientAllocationOwner } from "../src/client-allocation"
import { expect, test } from "bun:test"
import { MAX_PENDING_QUESTION_REQUESTS, MAX_SESSION_CONTROLS_PREPARED_BYTES } from "../../../protocol/types"
import { PROTOCOL_VERSION, type EngineEvent, type SessionControlsSnapshot } from "../src/protocol"
import { createInitialState, engineEvent, reduceRottweilerState } from "../src/state"
import { SessionSnapshotReader } from "../src/runtime-snapshots"

const meta = (sequence: string) => ({ protocol_version: PROTOCOL_VERSION, session_id: "s", sequence_id: sequence, emitted_at: "2026-01-01T00:00:00Z" })
const question = { question_id: "q", turn_id: "1", questions: [{ id: "q", prompt: "Choose", response_kind: "text" as const, options: [] }] }
function snapshot(through: string | null): SessionControlsSnapshot {
  return { through, controls: { questions: [question], approvals: [{ invocation_id: "inv", tool_call_id: "call", turn_id: "1", name: "write", args: { path: "file" }, capabilities: [], rationale: "write file", diff: null }], pending_plan: null } }
}
function ready(value: SessionControlsSnapshot): Extract<EngineEvent, { type: "session_controls_ready" }> {
  return { type: "session_controls_ready", session_id: "s", snapshot: value,
    meta: { protocol_version: PROTOCOL_VERSION, client_id: "c", request_id: "r", emitted_at: "2026-01-01T00:00:00Z" } }
}
const apply = (state: ReturnType<typeof createInitialState>, event: EngineEvent) => reduceRottweilerState(state, engineEvent(event), "s")

test("snapshot fence preserves unresolved controls through older replay while the cursor advances", () => {
  let state = apply(createInitialState(), ready(snapshot("4")))
  expect(state.lastSequence).toBeNull()
  state = apply(state, { type: "tool_call_started", meta: meta("1"), invocation_id: "inv", tool_call_id: "call", turn_id: "1", name: "write", args: {}, call_index: 0 })
  expect(state.tools.inv?.status).toBe("awaiting_approval")
  state = apply(state, { type: "question_answered", meta: meta("2"), turn_id: "1", question_id: "q", answers: [] })
  expect(state.questions.q).toBeDefined()
  state = apply(state, { type: "tool_approval_resolved", meta: meta("3"), invocation_id: "inv", tool_call_id: "call", turn_id: "1", decision: "allow_once" })
  expect(state.tools.inv?.status).toBe("awaiting_approval")
  state = apply(state, { type: "question_asked", meta: meta("4"), question_id: "stale", turn_id: "1", questions: question.questions })
  expect(state.questions.stale).toBeUndefined()
  expect(state.lastSequence).toBe("4")
  state = apply(state, { type: "tool_approval_resolved", meta: meta("5"), invocation_id: "inv", tool_call_id: "call", turn_id: "1", decision: "deny" })
  expect(state.tools.inv?.status).toBe("running")
  expect(state.tools.inv?.rationale).toBeNull()
  const resolved = state
  state = apply(state, ready(snapshot("4")))
  expect(state).toBe(resolved)
})

test("snapshot removal and invocation identity reject stale approval surfaces", () => {
  let state = apply(createInitialState(), ready(snapshot("4")))
  state = apply(state, ready({ through: "5", controls: { questions: [], approvals: [], pending_plan: null } }))
  expect(state.questions).toEqual({})
  expect(state.tools.inv?.status).toBe("running")
  expect(() => apply(state, ready({ ...snapshot("6"), controls: { ...snapshot("6").controls,
    approvals: [{ ...snapshot("6").controls.approvals[0]!, tool_call_id: "foreign" }],
  } }))).toThrow("invocation identity")
})

test("control admission rejects duplicate and oversized source payloads before replacing state", () => {
  const state = createInitialState()
  expect(() => apply(state, ready({ ...snapshot("1"), controls: { ...snapshot("1").controls, questions: [question, question] } }))).toThrow("duplicate")
  expect(() => apply(state, ready({ ...snapshot("1"), controls: { ...snapshot("1").controls,
    questions: Array.from({ length: MAX_PENDING_QUESTION_REQUESTS + 1 }, (_, i) => ({ ...question, question_id: String(i) })),
  } }))).toThrow("admission")
  expect(state.questions).toEqual({})
})

test("control reader coalesces demand and holds decoding ownership until cancelled work settles", async () => {
  const owner = new ClientAllocationOwner()
  const applied: string[] = [], started: string[] = [], settle: Array<() => void> = []
  const reader = new SessionSnapshotReader(() => owner, MAX_SESSION_CONTROLS_PREPARED_BYTES, async (sessionId, _signal, allocation) => {
    started.push(sessionId)
    allocation.admit(MAX_SESSION_CONTROLS_PREPARED_BYTES)
    await new Promise<void>(resolve => settle.push(resolve))
    return { ...ready(snapshot("1")), session_id: sessionId }
  }, event => { applied.push(event.session_id); return true }, error => { throw error })
  const first = new AbortController(), next = new AbortController()
  const completion = reader.refresh("old", first.signal)
  await Bun.sleep(0)
  first.abort()
  expect(owner.usage.domains.decoding).toBe(MAX_SESSION_CONTROLS_PREPARED_BYTES)
  reader.refresh("discarded", next.signal)
  reader.refresh("new", next.signal)
  expect(started).toEqual(["old"])
  settle[0]?.()
  while (started.length < 2) await Bun.sleep(1)
  expect(started).toEqual(["old", "new"])
  settle[1]?.()
  await completion
  expect(applied).toEqual(["new"])
  expect(owner.usage.bytes).toBe(0)
})

test("control reader rejects excess decoder reservation without applying a payload", async () => {
  const errors: unknown[] = [], applied: EngineEvent[] = []
  const reader = new SessionSnapshotReader(() => new ClientAllocationOwner(), MAX_SESSION_CONTROLS_PREPARED_BYTES, async (_session, _signal, allocation) => {
    allocation.admit(MAX_SESSION_CONTROLS_PREPARED_BYTES + 1)
    return ready(snapshot("1"))
  }, event => { applied.push(event); return true }, error => errors.push(error))
  await reader.refresh("s", new AbortController().signal)
  expect(applied).toEqual([])
  expect(errors).toHaveLength(1)
})

test("a snapshot losing the durable race is retried by the same bounded owner", async () => {
  const controller = new AbortController()
  let reads = 0, active = 0, peak = 0
  const reader = new SessionSnapshotReader(() => new ClientAllocationOwner(), MAX_SESSION_CONTROLS_PREPARED_BYTES, async () => {
    active++; peak = Math.max(peak, active)
    await Bun.sleep(0)
    active--; reads++
    return ready(snapshot(String(reads)))
  }, () => reads === 2, error => { throw error })
  await reader.refresh("s", controller.signal)
  expect(reads).toBe(2)
  expect(peak).toBe(1)
})
