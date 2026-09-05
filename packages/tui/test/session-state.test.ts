import { expect, test } from "bun:test"
import type { EngineEvent, SessionStateSnapshot } from "../src/protocol"
import { PROTOCOL_VERSION } from "../src/protocol"
import { createInitialState, engineEvent, reduceRottweilerState } from "../src/state"

const meta = (sequence: string) => ({ protocol_version: PROTOCOL_VERSION, session_id: "s", sequence_id: sequence, emitted_at: "2026-01-01T00:00:00Z" })
const apply = (state: ReturnType<typeof createInitialState>, event: EngineEvent) => reduceRottweilerState(state, engineEvent(event), "s")
function snapshot(through: string): SessionStateSnapshot {
  return { through, driver_client_id: "driver", title: "Session", model_alias: "main", provider: "provider", thinking: "high", mode_id: "execute",
    active_turn: { turn_id: "2", started: "5" }, completed_turns: "1", shell: null, compaction: null,
    queued_messages: [{ position: "2", preview: "next task", truncated: false }], budget: null }
}
function ready(snapshot: SessionStateSnapshot): EngineEvent {
  return { type: "session_state_ready", session_id: "s", snapshot,
    meta: { protocol_version: PROTOCOL_VERSION, client_id: "driver", request_id: "state", emitted_at: "2026-01-01T00:00:00Z" } }
}

test("scalar snapshot restores active state independently of the durable cursor and replayed metadata", () => {
  let state = apply(createInitialState(), ready(snapshot("5")))
  expect(state.lastSequence).toBeNull()
  expect(state.turns["2"]?.status).toBe("running")
  expect(state.hasActivity).toBe(true)
  state = apply(state, { type: "model_changed", meta: meta("2"), model: "earlier", provider: "old", thinking: "off" })
  state = apply(state, { type: "driver_changed", meta: meta("3"), driver_client_id: "earlier-driver" })
  state = apply(state, { type: "queued_messages_cleared", meta: meta("4") })
  state = apply(state, { type: "turn_started", meta: meta("5"), turn_id: "2" })
  expect(state.lastSequence).toBe("5")
  expect(state.model).toBe("main")
  expect(state.driverClientId).toBe("driver")
  expect(state.queuedMessages).toEqual([{ position: "2", content: "next task" }])
  state = apply(state, { type: "model_changed", meta: meta("6"), model: "new", provider: "new-provider", thinking: "high" })
  expect(state.model).toBe("new")
  expect(apply(state, ready(snapshot("5")))).toBe(state)
})

test("snapshot keeps a newer transient compaction attempt and does not fabricate a turn source", () => {
  const initial = createInitialState()
  const state = { ...initial, compaction: { ...initial.compaction, active: true, summaryTurnId: "compact", attempt: 3, text: "current" } }
  const next = apply(state, ready({ ...snapshot("10"), active_turn: { turn_id: "compact", started: null },
    compaction: { summary_turn_id: "compact", started: "9", attempt: 2 } }))
  expect(next.compaction).toBe(state.compaction)
  expect(next.recovery.activeTurnSource).toBeNull()
  expect(next.streamingTail).toBeNull()
})
