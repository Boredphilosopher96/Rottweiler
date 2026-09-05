import { describe, expect, test } from "bun:test"

import { contractFixture } from "../../../protocol/fixtures/contract"
import {
  PROTOCOL_VERSION,
  type ClientCommand,
  type EngineEvent,
  type Turn,
} from "../../../protocol/types"
import { createInitialState, engineEvent, reduceRottweilerState } from "../src/state"

interface ContractFixture {
  turns: Turn[]
  client_commands: ClientCommand[]
  engine_events: EngineEvent[]
}

const tsAuthoredCommand = {
  type: "send_message",
  meta: {
    protocol_version: PROTOCOL_VERSION,
    client_id: "client-fixture",
    request_id: "request-fixture",
  },
  session_id: "session-fixture",
  content: "Build it",
  attachments: [],
} satisfies ClientCommand

const tsAuthoredEvent = {
  type: "text_delta",
  meta: {
    protocol_version: PROTOCOL_VERSION,
    session_id: "session-fixture",
    sequence_id: "4",
    emitted_at: "2026-01-01T00:00:00Z",
    caused_by: null,
  },
  turn_id: "turn-fixture",
  text: "hello",
} satisfies EngineEvent

describe("generated Rust/TypeScript protocol contract", () => {
  test("type-checks and round-trips the complete Rust-authored fixture", async () => {
    const fixture: ContractFixture = contractFixture
    const fixtureUrl = new URL("../../../protocol/fixtures/contract.json", import.meta.url)
    const fixtureJson: unknown = await Bun.file(fixtureUrl).json()

    expect(fixture.client_commands).toContainEqual(tsAuthoredCommand)
    expect(fixture.engine_events).toContainEqual(tsAuthoredEvent)
    expect(fixtureJson).toEqual(fixture)

    const roundTripped: unknown = JSON.parse(JSON.stringify(fixture))
    expect(roundTripped).toEqual(fixture)
  })

  test("classifies every Rust-authored fixture event as generated protocol", () => {
    for (const event of contractFixture.engine_events) {
      const state = reduceRottweilerState(createInitialState(), engineEvent(event))
      expect(state.protocol.unknownEvents).toBe(0)
    }
  })
})
