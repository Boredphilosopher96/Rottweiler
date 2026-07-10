import { describe, expect, test } from "bun:test"

import {
  PROTOCOL_VERSION,
  type ContextSnapshot,
  type CostSnapshot,
  type EngineEvent,
  type Turn,
} from "../src/protocol"
import {
  createInitialState,
  engineEvent,
  reduceRottweilerState,
  transportConnected,
  type RottweilerState,
} from "../src/state"
import type { WireEngineEvent } from "../src/transport"

function meta(sequence: string) {
  return {
    protocol_version: PROTOCOL_VERSION,
    session_id: "session-state",
    sequence_id: sequence,
    emitted_at: "2026-01-01T00:00:00Z",
  }
}

function reduce(state: RottweilerState, event: WireEngineEvent): RottweilerState {
  return reduceRottweilerState(state, engineEvent(event))
}

describe("pure TUI state reducer", () => {
  test("gap replay converges to the same projection as an uninterrupted stream", () => {
    const events: EngineEvent[] = [
      { type: "mode_changed", meta: meta("1"), mode: "plan" },
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

  test("keeps immutable transcript history stable while updating the streaming tail", () => {
    let state = createInitialState()
    state = reduce(state, { type: "turn_started", meta: meta("1"), turn_id: "7" })
    const transcript = state.transcript
    state = reduce(state, { type: "text_delta", meta: meta("2"), turn_id: "7", text: "hel" })
    expect(state.transcript).toBe(transcript)
    expect(state.streamingTail?.text).toBe("hel")
    state = reduce(state, { type: "text_delta", meta: meta("3"), turn_id: "7", text: "lo" })
    expect(state.transcript).toBe(transcript)
    expect(state.streamingTail?.text).toBe("hello")

    const turn: Turn = {
      role: "assistant",
      blocks: [{ type: "text", text: "hello" }],
      meta: { synthetic: false, summary: false },
    }
    state = reduce(state, {
      type: "conversation_turn_committed",
      meta: meta("4"),
      agent_turn: "7",
      turn,
    })
    expect(state.transcript).toEqual([{ sequenceId: "4", agentTurn: "7", turn }])
    expect(state.streamingTail).toBeNull()
  })

  test("correlates command replies without moving the durable cursor", () => {
    const state = reduce(createInitialState(), {
      type: "command_acknowledged",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client",
        request_id: "request-1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-state",
      outcome: { type: "accepted" },
    })

    expect(state.lastSequence).toBeNull()
    expect(state.commandAcks["request-1"]).toEqual({
      requestId: "request-1",
      responseType: "command_acknowledged",
      outcome: { type: "accepted" },
      sessionId: "session-state",
    })
  })

  test("projects turns, tools, questions, snapshots, mode, model, and shell state", () => {
    const context = {
      turn_id: "4",
      stable_prefix_hash: "stable",
      used_tokens: "10",
      usable_tokens: "100",
      reserved_tokens: "20",
      cache_breakpoints: [],
      items: [],
    } satisfies ContextSnapshot
    const usage = {
      input_tokens: "2",
      output_tokens: "3",
      cache_read_tokens: "0",
      cache_write_tokens: "0",
      reasoning_tokens: "0",
    }
    const cost = {
      utc_day: "2026-01-01",
      turns: [],
      session_usage: usage,
      session_cost_micros_usd: "5",
      session_ai_credit_micros: "0",
      daily_cost_micros_usd: "5",
      daily_ai_credit_micros: "0",
      trailing_minute_cost_micros_usd: "5",
      trailing_minute_ai_credit_micros: "0",
      cache_hit_basis_points: 0,
      session_cost_cap_micros_usd: null,
      daily_cost_cap_micros_usd: null,
      session_ai_credit_cap_micros: null,
      daily_ai_credit_cap_micros: null,
      spend_rate_alarm_micros_usd_per_minute: null,
      ai_credit_rate_alarm_micros_per_minute: null,
      hard_cap_reached: false,
      session_monetary_accounting_complete: true,
      daily_monetary_accounting_complete: true,
      session_subscription_quota_entries: "0",
      session_cost_unavailable_entries: "0",
      session_non_usd_monetary_entries: "0",
      daily_subscription_quota_entries: "0",
      daily_cost_unavailable_entries: "0",
      daily_non_usd_monetary_entries: "0",
    } satisfies CostSnapshot
    const ackMeta = (requestId: string) => ({
      protocol_version: PROTOCOL_VERSION,
      client_id: "client",
      request_id: requestId,
      emitted_at: "2026-01-01T00:00:00Z",
    })

    let state = createInitialState()
    state = reduce(state, {
      type: "context_snapshot_ready",
      meta: ackMeta("context"),
      session_id: "session-state",
      snapshot: context,
    })
    state = reduce(state, {
      type: "cost_snapshot_ready",
      meta: ackMeta("cost"),
      session_id: "session-state",
      snapshot: cost,
    })
    const durable: EngineEvent[] = [
      { type: "mode_changed", meta: meta("1"), mode: "execute" },
      { type: "model_changed", meta: meta("2"), model: "fast" },
      { type: "turn_started", meta: meta("3"), turn_id: "4" },
      {
        type: "tool_call_started",
        meta: meta("4"),
        turn_id: "4",
        tool_call_id: "tool-1",
        name: "read",
        args: { path: "README.md" },
        call_index: 0,
      },
      {
        type: "tool_approval_needed",
        meta: meta("5"),
        turn_id: "4",
        tool_call_id: "tool-1",
        name: "read",
        args: { path: "README.md" },
        capabilities: ["read_filesystem"],
        rationale: "fixture",
      },
      {
        type: "tool_output_delta",
        meta: meta("6"),
        turn_id: "4",
        tool_call_id: "tool-1",
        stream: "stdout",
        chunk: "live",
      },
      {
        type: "tool_call_finished",
        meta: meta("7"),
        turn_id: "4",
        tool_call_id: "tool-1",
        output: { type: "text", text: "done" },
        is_error: false,
        call_index: 0,
      },
      {
        type: "question_asked",
        meta: meta("8"),
        turn_id: "4",
        question_id: "question-1",
        questions: [
          {
            id: "question-1",
            prompt: "Continue?",
            response_kind: "select_one",
            options: [{ value: "yes", label: "Yes" }],
          },
        ],
      },
      {
        type: "question_answered",
        meta: meta("9"),
        turn_id: "4",
        question_id: "question-1",
        answers: [{ question_id: "question-1", values: ["yes"] }],
      },
      {
        type: "turn_finished",
        meta: meta("10"),
        turn_id: "4",
        status: "completed",
        usage,
        cost: { kind: "monetary", amount_micros: "5", currency: "USD" },
      },
      {
        type: "user_shell_state_changed",
        meta: meta("11"),
        shell_id: "shell-1",
        active: false,
        status: 0,
        captured_output: "ok",
      },
    ]
    for (const event of durable) {
      state = reduce(state, event)
    }

    expect(state.context).toBe(context)
    expect(state.cost).toBe(cost)
    expect(state.mode).toBe("execute")
    expect(state.model).toBe("fast")
    expect(state.turns["4"]).toMatchObject({ status: "completed", usage })
    expect(state.tools["tool-1"]).toMatchObject({
      status: "finished",
      rationale: "fixture",
      chunks: [{ stream: "stdout", chunk: "live" }],
      output: { type: "text", text: "done" },
    })
    expect(state.questions["question-1"]).toMatchObject({ answered: true })
    expect(state.shell).toEqual({
      shellId: "shell-1",
      active: false,
      status: 0,
      capturedOutput: "ok",
    })
  })
})
