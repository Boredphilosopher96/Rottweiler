import type { EngineEvent } from "../../src/protocol"
import { describe, expect, test } from "bun:test"
import {
  type Turn
} from "../../src/protocol"
import {
  createInitialState,
  enterReplayMode,
  MAX_COMPACTION_STREAM_BYTES,
  MAX_RETAINED_TURN_PROJECTIONS
} from "../../src/state"

import { meta, metaAt, reduce } from "./fixtures"

describe("state streaming", () => {

  test("retires live text when its semantic conversation source commits", () => {
    let state = createInitialState()
    state = reduce(state, { type: "turn_started", meta: meta("1"), turn_id: "7" })
    const activity = state.hasActivity
    state = reduce(state, { type: "text_delta", meta: meta("2"), turn_id: "7", text: "hel" })
    expect(state.hasActivity).toBe(activity)
    expect(state.streamingTail?.text).toBe("hel")
    state = reduce(state, { type: "text_delta", meta: meta("3"), turn_id: "7", text: "lo" })
    expect(state.hasActivity).toBe(activity)
    expect(state.streamingTail?.text).toBe("hello")

    const turn: Turn = {
      role: "assistant",
      blocks: [{ type: "text", text: "hello" }],
      meta: { model: "copilot/gpt-5-mini", synthetic: false, summary: false },
    }
    state = reduce(state, {
      type: "conversation_turn_committed",
      meta: meta("4"),
      agent_turn: "7",
      turn,
    })
    expect(state.hasActivity).toBe(true)
    expect("transcript" in state).toBe(false)
    expect(state.streamingTail).toBeNull()
    expect(state.model).toBe("copilot/gpt-5-mini")
    expect(state.provider).toBe("copilot")
  })

  test("streams compaction attempts separately and resets discarded fallback text", () => {
    let state = reduce(createInitialState(), {
      type: "compaction_started",
      meta: meta("1"),
      reason: "automatic",
    })
    state = reduce(state, {
      type: "compaction_attempt_started",
      session_id: "session-state",
      summary_turn_id: "7",
      attempt: 0,
    })
    state = reduce(state, {
      type: "compaction_thinking_delta",
      session_id: "session-state",
      summary_turn_id: "7",
      attempt: 0,
      text: "Inspecting ",
    })
    state = reduce(state, {
      type: "compaction_text_delta",
      session_id: "session-state",
      summary_turn_id: "7",
      attempt: 0,
      text: "Old partial",
    })
    expect(state.compaction).toMatchObject({
      active: true,
      attempt: 0,
      thinking: "Inspecting ",
      text: "Old partial",
    })

    state = reduce(state, {
      type: "compaction_attempt_started",
      session_id: "session-state",
      summary_turn_id: "7",
      attempt: 1,
    })
    expect(state.compaction).toMatchObject({ attempt: 1, thinking: "", text: "" })
    state = reduce(state, {
      type: "compaction_text_delta",
      session_id: "session-state",
      summary_turn_id: "7",
      attempt: 1,
      text: "## Fresh summary",
    })
    expect(state.compaction).toMatchObject({
      attempt: 1,
      thinking: "",
      text: "## Fresh summary",
    })
    state = reduce(state, {
      type: "compaction_finished",
      meta: meta("2"),
      summary_turn_id: "7",
      reclaimed_tokens: "1200",
    })
    expect(state.compaction).toMatchObject({
      active: false,
      attempt: null,
      text: "",
      thinking: "",
      reclaimedTokens: "1200",
    })
  })

  test("only a correlated compaction terminal event clears streamed progress", () => {
    let state = reduce(createInitialState(), {
      type: "compaction_started",
      meta: meta("1"),
      reason: "manual",
    })
    state = reduce(state, {
      type: "compaction_attempt_started",
      session_id: "session-state",
      summary_turn_id: "9",
      attempt: 0,
    })
    state = reduce(state, {
      type: "compaction_text_delta",
      session_id: "session-state",
      summary_turn_id: "9",
      attempt: 0,
      text: "partial",
    })
    state = reduce(state, {
      type: "compaction_text_delta",
      session_id: "session-state",
      summary_turn_id: "8",
      attempt: 0,
      text: "stale turn",
    })
    state = reduce(state, {
      type: "compaction_thinking_delta",
      session_id: "session-state",
      summary_turn_id: "9",
      attempt: 1,
      text: "stale attempt",
    })
    state = reduce(state, {
      type: "compaction_failed",
      meta: meta("2"),
      summary_turn_id: "8",
    })
    expect(state.compaction).toMatchObject({ active: true, text: "partial", thinking: "" })
    state = reduce(state, {
      type: "error",
      meta: meta("3"),
      error: {
        category: "provider",
        code: "unrelated",
        message: "another operation failed",
        retryable: false,
      },
    })
    expect(state.compaction).toMatchObject({ active: true, text: "partial" })
    state = reduce(state, {
      type: "compaction_failed",
      meta: meta("4"),
      summary_turn_id: "9",
    })
    expect(state.compaction).toMatchObject({ active: false, text: "", thinking: "" })
  })

  test("bounds connection-scoped compaction text and reasoning", () => {
    let state = reduce(createInitialState(), {
      type: "compaction_started",
      meta: meta("1"),
      reason: "automatic",
    })
    state = reduce(state, {
      type: "compaction_attempt_started",
      session_id: "session-state",
      summary_turn_id: "11",
      attempt: 0,
    })
    const oversized = "界".repeat(MAX_COMPACTION_STREAM_BYTES)
    state = reduce(state, {
      type: "compaction_text_delta",
      session_id: "session-state",
      summary_turn_id: "11",
      attempt: 0,
      text: oversized,
    })
    state = reduce(state, {
      type: "compaction_thinking_delta",
      session_id: "session-state",
      summary_turn_id: "11",
      attempt: 0,
      text: oversized,
    })

    const encoder = new TextEncoder()
    expect(encoder.encode(state.compaction.text).byteLength).toBeLessThanOrEqual(
      MAX_COMPACTION_STREAM_BYTES,
    )
    expect(encoder.encode(state.compaction.thinking).byteLength).toBeLessThanOrEqual(
      MAX_COMPACTION_STREAM_BYTES,
    )
  })

  test("replays the same timing deterministically and never substitutes wall time", () => {
    const events: EngineEvent[] = [
      {
        type: "turn_started",
        meta: metaAt("1", "2026-01-01T12:00:00.000Z"),
        turn_id: "replay-turn",
      },
      {
        type: "tool_call_started",
        meta: metaAt("2", "2026-01-01T12:00:01.000Z"),
        turn_id: "replay-turn",
        tool_call_id: "replay-tool",
        invocation_id: "replay-tool",
        name: "read",
        args: { path: "README.md" },
        call_index: 0,
      },
      {
        type: "tool_output_delta",
        meta: metaAt("3", "2026-01-01T12:00:03.000Z"),
        turn_id: "replay-turn",
        tool_call_id: "replay-tool",
        invocation_id: "replay-tool",
        stream: "stdout",
        chunk: "retained",
      },
    ]
    const replayed = events.reduce(
      (state, event) => reduce(state, event),
      enterReplayMode(createInitialState(), "session-state"),
    )
    const repeated = events.reduce(
      (state, event) => reduce(state, event),
      enterReplayMode(createInitialState(), "session-state"),
    )
    expect(replayed.tools["replay-tool"]?.timing).toEqual(repeated.tools["replay-tool"]?.timing)
    expect(replayed.tools["replay-tool"]?.timing).toEqual({
      kind: "open",
      startedAtMs: Date.parse("2026-01-01T12:00:01.000Z"),
      lastObservedAtMs: Date.parse("2026-01-01T12:00:03.000Z"),
    })

    let malformed = reduce(createInitialState(), {
      type: "turn_started",
      meta: metaAt("1", "not-a-timestamp"),
      turn_id: "malformed-turn",
    })
    malformed = reduce(malformed, {
      type: "tool_call_started",
      meta: metaAt("2", "still-not-a-timestamp"),
      turn_id: "malformed-turn",
      tool_call_id: "malformed-tool",
      invocation_id: "malformed-tool",
      name: "read",
      args: { path: "README.md" },
      call_index: 0,
    })
    expect(malformed.turns["malformed-turn"]?.timing).toEqual({ kind: "unknown" })
    expect(malformed.tools["malformed-tool"]?.timing).toEqual({ kind: "unknown" })
  })

  test("bounds retained transcript and completed turn history", () => {
    let state = createInitialState()
    const total = MAX_RETAINED_TURN_PROJECTIONS + 8
    for (let index = 0; index < total; index += 1) {
      const turnId = `${index + 1}`
      const sequence = index * 3
      state = reduce(state, {
        type: "turn_started",
        meta: meta(`${sequence + 1}`),
        turn_id: turnId,
      })
      state = reduce(state, {
        type: "conversation_turn_committed",
        meta: meta(`${sequence + 2}`),
        agent_turn: turnId,
        turn: {
          role: "assistant",
          blocks: [{ type: "text", text: `turn ${turnId}` }],
          meta: { synthetic: false, summary: false },
        },
      })
      state = reduce(state, {
        type: "turn_finished",
        meta: meta(`${sequence + 3}`),
        turn_id: turnId,
        status: "completed",
        usage: {
          input_tokens: "1",
          output_tokens: "1",
          cache_read_tokens: "0",
          cache_write_tokens: "0",
          reasoning_tokens: "0",
        },
        cost: { kind: "unavailable", reason: "fixture" },
      })
    }

    expect(state.hasActivity).toBe(true)
    expect("transcript" in state).toBe(false)
    expect(Object.keys(state.turns)).toHaveLength(MAX_RETAINED_TURN_PROJECTIONS)
    expect(state.turns["1"]).toBeUndefined()
    expect(state.turns[`${total}`]?.status).toBe("completed")
  })
})
