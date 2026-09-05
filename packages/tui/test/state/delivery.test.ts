import { describe, expect, test } from "bun:test"
import {
  type EngineEvent
} from "../../src/protocol"
import {
  createInitialState,
  reduceRottweilerState,
  transportConnected
} from "../../src/state"
import { meta, reduce } from "./fixtures"

describe("state delivery", () => {

  test("gap replay converges to the same projection as an uninterrupted stream", () => {
    const events: EngineEvent[] = [
      { type: "mode_changed", meta: meta("1"), mode: "plan", definition_fingerprint: "fixture" },
      { type: "model_changed", meta: meta("2"), model: "fast" },
      { type: "user_shell_state_changed", meta: meta("3"), shell_id: "shell-1", active: true },
    ]
    let live = reduceRottweilerState(createInitialState(), transportConnected(0))
    for (const event of events) {
      live = reduce(live, event)
    }

    let replay = reduceRottweilerState(createInitialState(), transportConnected(0))
    replay = reduce(replay, events[0]!)
    replay = reduce(replay, events[2]!)
    expect(replay.connection).toMatchObject({
      phase: "replaying",
      gap: { expected: "2", received: "3" },
    })
    replay = reduce(replay, events[1]!)
    replay = reduce(replay, events[2]!)

    expect(replay).toEqual(live)
  })

  test("compares full u64 sequence ids, suppresses duplicates, and advances unknown events", () => {
    let state = createInitialState()
    state = reduce(state, {
      type: "mode_changed",
      meta: meta("18446744073709551614"),
      mode: "plan",
      definition_fingerprint: "fixture",
    })
    state = reduce(state, {
      type: "model_changed",
      meta: meta("18446744073709551615"),
      model: "fast",
    })
    state = reduce(state, {
      type: "model_changed",
      meta: meta("18446744073709551615"),
      model: "ignored-duplicate",
    })
    state = reduce(state, {
      type: "mode_changed",
      meta: meta("18446744073709551616"),
      mode: "invalid",
      definition_fingerprint: "fixture",
    })

    expect(state.lastSequence).toBe("18446744073709551615")
    expect(state.model).toBe("fast")
    expect(state.protocol).toMatchObject({ duplicateEvents: 1, invalidEvents: 1 })

    const unknown = reduce(createInitialState(), {
      type: "future_additive_event",
      meta: meta("1"),
      additive_field: true,
    })
    expect(unknown.lastSequence).toBe("1")
    expect(unknown.protocol).toMatchObject({
      unknownEvents: 1,
      lastUnknownType: "future_additive_event",
    })
  })
})
