import { expect, test } from "bun:test"
import { PROTOCOL_VERSION, type EngineEvent } from "../src/protocol"
import { createInitialState, engineEvent, enterReplayMode, reduceRottweilerState } from "../src/state"
import { isWireEngineEvent } from "../src/transport"

test("history availability does not claim durable replay or advance a cursor", () => {
  const initial = enterReplayMode(createInitialState(), "historical")
  const event = {
    type: "session_history_ready",
    meta: { protocol_version: PROTOCOL_VERSION, client_id: "reader", request_id: "history", emitted_at: "2026-09-04T00:00:00Z" },
    session_id: "historical", through_sequence: "18446744073709551615",
  } satisfies EngineEvent
  expect(isWireEngineEvent(event)).toBe(true)
  const state = reduceRottweilerState(initial, engineEvent(event))
  expect(state.historyReady).toEqual({ sessionId: "historical", through: "18446744073709551615" })
  expect(state.lastSequence).toBeNull()
  expect(state.replay.completedThrough).toBeNull()
  expect(state.transcript).toBe(initial.transcript)
  expect(state.commandAcks).toBe(initial.commandAcks)
})
