import { expect, test } from "bun:test"

import { PROTOCOL_VERSION, type EngineEvent } from "../src/protocol"
import { createInitialState, engineEvent, reduceRottweilerState } from "../src/state"
import { isWireEngineEvent } from "../src/transport"

const meta = (sequence: string) => ({
  protocol_version: PROTOCOL_VERSION,
  session_id: "session-accounting",
  sequence_id: sequence,
  emitted_at: "2026-01-01T00:00:00Z",
})

const actuals = {
  usage: {
    input_tokens: "100",
    output_tokens: "20",
    cache_read_tokens: "80",
    cache_write_tokens: "0",
    reasoning_tokens: "5",
  },
  cost: { kind: "monetary", amount_micros: "125", currency: "USD" },
} as const

test("provider receipts advance the durable cursor without duplicating turn display accounting", () => {
  const initial = reduceRottweilerState(createInitialState(), engineEvent({
    type: "turn_started",
    meta: meta("0"),
    turn_id: "turn-accounting",
  }))
  const receipt = {
    type: "provider_call_accounted",
    meta: meta("1"),
    call: {
      session_id: "session-accounting",
      turn_id: "turn-accounting",
      attribution: "main",
      call_id: "provider-call-accounting",
      attempt: 0,
    },
    actuals,
  } satisfies EngineEvent
  expect(isWireEngineEvent(receipt)).toBe(true)
  const accounted = reduceRottweilerState(initial, engineEvent(receipt))
  expect(accounted.lastSequence).toBe("1")
  expect(accounted.turns).toBe(initial.turns)
  expect(accounted.transcript).toBe(initial.transcript)
  expect(accounted.streamingTail).toBe(initial.streamingTail)
  expect(accounted.cost).toBe(initial.cost)

  const finished = {
    type: "turn_finished",
    meta: meta("2"),
    turn_id: "turn-accounting",
    status: "completed",
    ...actuals,
  } satisfies EngineEvent
  const completed = reduceRottweilerState(accounted, engineEvent(finished))
  expect(completed.turns["turn-accounting"]?.cost).toEqual(actuals.cost)
  const duplicate = reduceRottweilerState(completed, engineEvent(finished))
  expect(duplicate.turns).toBe(completed.turns)
  expect(duplicate.transcript).toBe(completed.transcript)
  expect(duplicate.cost).toBe(completed.cost)
  expect(duplicate.protocol.duplicateEvents).toBe(1)
})
