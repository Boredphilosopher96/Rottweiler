import { describe, expect, test } from "bun:test"

import {
  PROTOCOL_VERSION,
  type ContextSnapshot,
  type CostSnapshot,
  type EngineEvent,
  type SubagentResult,
  type Turn,
} from "../src/protocol"
import {
  createInitialState,
  engineEvent,
  MAX_SUBAGENT_TASK_BYTES,
  MAX_TERMINAL_SUBAGENT_HISTORY,
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

function childResult(
  subagentId: string,
  sessionId: string,
  finalText: string,
  status: SubagentResult["status"] = "completed",
): SubagentResult {
  return {
    subagent_id: subagentId,
    session_id: sessionId,
    status,
    final_text: finalText,
    touched_files: [],
    diff_artifact: null,
    usage: {
      input_tokens: "1",
      output_tokens: "1",
      cache_read_tokens: "0",
      cache_write_tokens: "0",
      reasoning_tokens: "0",
    },
    cost: { kind: "unavailable", reason: "fixture" },
    turns: "1",
    duration_millis: "1",
  }
}

describe("pure TUI state reducer", () => {
  test("projects plugin status and bounded UI notifications as known durable events", () => {
    let state = reduce(createInitialState(), {
      type: "plugin_status_changed",
      meta: meta("1"),
      plugin_id: "formatter",
      status: "watching",
    })
    state = reduce(state, {
      type: "ui_notification",
      meta: meta("2"),
      plugin_id: "formatter",
      title: "Format complete",
      message: "src/main.rs",
    })
    state = reduce(state, {
      type: "plugin_message_injected",
      meta: meta("3"),
      plugin_id: "formatter",
      content: "/help remains plain text",
      queued: true,
    })

    expect(state.pluginStatuses).toEqual({ formatter: "watching" })
    expect(state.pluginNotifications).toEqual([
      { pluginId: "formatter", title: "Format complete", message: "src/main.rs" },
    ])
    expect(state.protocol.unknownEvents).toBe(0)
  })

  test("projects live workspace-root generations using only virtual paths", () => {
    const state = reduce(createInitialState(), {
      type: "workspace_roots_changed",
      meta: meta("1"),
      generation: "1",
      effective_from_turn: "4",
      roots: [
        { index: 0, path: "@root/0", machine_local: false },
        { index: 1, path: "@root/1", machine_local: false },
      ],
    })
    expect(state.workspaceRoots).toEqual({
      generation: "1",
      effectiveFromTurn: "4",
      roots: ["@root/0", "@root/1"],
    })
  })

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

  test("projects subagent lifecycle in deterministic spawn order and retains turn history", () => {
    let state = reduce(createInitialState(), {
      type: "turn_started",
      meta: meta("1"),
      turn_id: "7",
    })
    state = reduce(state, {
      type: "subagent_spawned",
      meta: meta("2"),
      subagent_id: "child-b",
      child_session_id: "session-child-b",
      task: "Inspect providers",
    })
    state = reduce(state, {
      type: "subagent_spawned",
      meta: meta("3"),
      subagent_id: "child-a",
      child_session_id: "session-child-a",
      task: "Inspect tools",
    })
    state = reduce(state, {
      type: "subagent_finished",
      meta: meta("4"),
      subagent_id: "child-b",
      result: childResult("child-b", "session-child-b", "Provider notes"),
      output: { type: "text", text: "Provider notes" },
      is_error: false,
    })

    expect(state.subagentOrder).toEqual(["child-b", "child-a"])
    expect(state.subagents["child-b"]).toMatchObject({
      task: "Inspect providers",
      status: "completed",
      summary: "Provider notes",
    })
    expect(state.subagents["child-a"]?.status).toBe("running")

    state = reduce(state, { type: "turn_started", meta: meta("5"), turn_id: "8" })
    expect(state.subagentOrder).toEqual(["child-b", "child-a"])
    expect(state.subagents["child-b"]?.parentTurnId).toBe("7")
    state = reduce(state, {
      type: "subagent_spawned",
      meta: meta("6"),
      subagent_id: "child-b",
      child_session_id: "session-child-b",
      task: "Follow up on providers",
    })
    expect(state.subagents["child-b"]).toMatchObject({
      parentTurnId: "8",
      status: "running",
      task: "Follow up on providers",
    })
    expect(state.subagents["child-b@7"]).toMatchObject({
      parentTurnId: "7",
      status: "completed",
      summary: "Provider notes",
    })
    expect(state.subagentOrder).toEqual(["child-b@7", "child-a", "child-b"])
  })

  test("coalesces connection-scoped child progress without advancing the parent cursor", () => {
    let state = reduce(createInitialState(), {
      type: "subagent_spawned",
      meta: meta("1"),
      subagent_id: "child",
      child_session_id: "session-child",
      task: "Inspect orchestration",
    })
    state = reduce(state, {
      type: "subagent_progress",
      parent_session_id: "session-state",
      subagent_id: "child",
      child_session_id: "session-child",
      child_sequence: "9",
      event: { type: "tool_call_started", name: "grep" },
    })
    expect(state.lastSequence).toBe("1")
    expect(state.subagents.child).toMatchObject({
      childSessionId: "session-child",
      activity: "using tool · grep",
      status: "running",
    })

    state = reduce(state, {
      type: "subagent_progress",
      parent_session_id: "session-state",
      subagent_id: "child",
      child_session_id: "session-child",
      child_sequence: "10",
      event: { type: "text_delta", text: "first" },
    })
    const writing = state
    state = reduce(state, {
      type: "subagent_progress",
      parent_session_id: "session-state",
      subagent_id: "child",
      child_session_id: "session-child",
      child_sequence: "11",
      event: { type: "text_delta", text: " second" },
    })
    expect(state).not.toBe(writing)
    expect(state.lastSequence).toBe("1")
    expect(state.subagents.child?.activity).toBe("writing response")
    expect(state.subagents.child?.lastChildSequence).toBe("11")

    const current = state
    state = reduce(state, {
      type: "subagent_progress",
      parent_session_id: "session-state",
      subagent_id: "child",
      child_session_id: "session-child",
      child_sequence: "10",
      event: { type: "tool_call_started", name: "stale" },
    })
    expect(state).toBe(current)
    state = reduce(state, {
      type: "subagent_progress",
      parent_session_id: "session-state",
      subagent_id: "child",
      child_session_id: "wrong-session",
      child_sequence: "12",
      event: { type: "tool_call_started", name: "unsafe" },
    })
    expect(state.protocol.invalidEvents).toBe(1)
    state = reduce(state, {
      type: "subagent_progress",
      parent_session_id: "session-state",
      subagent_id: "unknown-child",
      child_session_id: "unknown-session",
      child_sequence: "1",
      event: { type: "thinking_delta", text: "unknown" },
    })
    expect(state.protocol.invalidEvents).toBe(2)
    expect(state.subagentOrder).toEqual(["child"])
  })

  test("extracts a bounded terminal summary without retaining a multi-megabyte diff", () => {
    let state = reduce(createInitialState(), {
      type: "turn_started",
      meta: meta("1"),
      turn_id: "5",
    })
    state = reduce(state, {
      type: "subagent_spawned",
      meta: meta("2"),
      subagent_id: "large-child",
      child_session_id: "large-session",
      task: "Return a large diff",
    })
    state = reduce(state, {
      type: "subagent_finished",
      meta: meta("3"),
      subagent_id: "large-child",
      is_error: false,
      output: { type: "text", text: "Large change complete" },
      result: {
        ...childResult("large-child", "large-session", "Large change complete"),
        touched_files: ["large.bin"],
        diff_artifact: {
          id: "large-diff",
          base_commit: "0".repeat(40),
          touched_files: [{ path: "large.bin", status: "modified" }],
          unified_diff: "x".repeat(4 * 1024 * 1024),
        },
      },
    })

    expect(state.subagents["large-child"]).toMatchObject({
      status: "completed",
      summary: "Large change complete",
      touchedFileCount: 1,
      diffArtifactId: "large-diff",
    })
    expect(JSON.stringify(state).length).toBeLessThan(50_000)
  })

  test("bounds large spawn tasks and archived follow-up history in retained state", () => {
    const runs = MAX_TERMINAL_SUBAGENT_HISTORY * 3
    const largeTask = `large task ${"界".repeat(22_000)}`
    const largeDiff = `UNRETAINED-DIFF-${"x".repeat(64 * 1024)}`

    const replay = (): RottweilerState => {
      let state = createInitialState()
      let sequence = 1
      for (let index = 0; index < runs; index += 1) {
        state = reduce(state, {
          type: "turn_started",
          meta: meta(String(sequence++)),
          turn_id: String(index + 1),
        })
        state = reduce(state, {
          type: "subagent_spawned",
          meta: meta(String(sequence++)),
          subagent_id: "continuable-child",
          child_session_id: "continuable-session",
          task: `${largeTask} follow-up ${index}`,
        })
        state = reduce(state, {
          type: "subagent_finished",
          meta: meta(String(sequence++)),
          subagent_id: "continuable-child",
          is_error: false,
          output: { type: "text", text: `completed ${index}` },
          result: {
            ...childResult(
              "continuable-child",
              "continuable-session",
              `completed ${index}`,
            ),
            touched_files: [`file-${index}.txt`],
            diff_artifact: {
              id: `artifact-${index}`,
              base_commit: "0".repeat(40),
              touched_files: [{ path: `file-${index}.txt`, status: "modified" }],
              unified_diff: largeDiff,
            },
          },
        })
      }

      state = reduce(state, {
        type: "turn_started",
        meta: meta(String(sequence++)),
        turn_id: String(runs + 1),
      })
      return reduce(state, {
        type: "subagent_spawned",
        meta: meta(String(sequence)),
        subagent_id: "continuable-child",
        child_session_id: "continuable-session",
        task: `${largeTask} current follow-up`,
      })
    }

    const state = replay()
    const projections = Object.values(state.subagents)
    const encoded = new TextEncoder()
    expect(projections).toHaveLength(MAX_TERMINAL_SUBAGENT_HISTORY + 1)
    expect(state.subagentOrder).toHaveLength(MAX_TERMINAL_SUBAGENT_HISTORY + 1)
    expect(
      projections.every(
        (projection) => encoded.encode(projection.task).byteLength <= MAX_SUBAGENT_TASK_BYTES,
      ),
    ).toBe(true)
    expect(state.subagents["continuable-child"]).toMatchObject({
      status: "running",
      parentTurnId: String(runs + 1),
    })
    expect(
      projections.some((projection) => projection.diffArtifactId === `artifact-${runs - 1}`),
    ).toBe(true)

    const retained = JSON.stringify(state)
    expect(retained).not.toContain("UNRETAINED-DIFF")
    expect(retained.length).toBeLessThan(300_000)
    expect(replay()).toEqual(state)
  })

  test("preserves every typed terminal subagent status", () => {
    const statuses = ["failed", "cancelled", "timed_out", "max_turns"] as const
    let state = reduce(createInitialState(), {
      type: "turn_started",
      meta: meta("1"),
      turn_id: "6",
    })
    let sequence = 2
    for (const status of statuses) {
      const subagentId = `child-${status}`
      const childSessionId = `session-${status}`
      state = reduce(state, {
        type: "subagent_spawned",
        meta: meta(String(sequence++)),
        subagent_id: subagentId,
        child_session_id: childSessionId,
        task: `Finish ${status}`,
      })
      state = reduce(state, {
        type: "subagent_finished",
        meta: meta(String(sequence++)),
        subagent_id: subagentId,
        result: childResult(subagentId, childSessionId, status, status),
        output: { type: "text", text: status },
        is_error: true,
      })
      expect(state.subagents[subagentId]?.status).toBe(status)
    }
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

  test("projects cumulative review replacements and bounded session search replies", () => {
    const replyMeta = (requestId: string) => ({
      protocol_version: PROTOCOL_VERSION,
      client_id: "client",
      request_id: requestId,
      emitted_at: "2026-01-01T00:00:00Z",
    })
    let state = reduce(createInitialState(), {
      type: "session_review_ready",
      meta: replyMeta("review-1"),
      session_id: "session-state",
      review: {
        session_id: "session-state",
        files: [
          {
            path: "src/lib.rs",
            unified_diff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n",
            status: "pending",
            truncated: false,
            unrestorable_reason: null,
            original_hash: "old-hash",
            current_hash: "new-hash",
          },
        ],
      },
    })
    expect(state.review?.files[0]).toMatchObject({
      path: "src/lib.rs",
      status: "pending",
      originalHash: "old-hash",
      currentHash: "new-hash",
    })
    state = reduce(state, {
      type: "session_review_updated",
      meta: replyMeta("review-2"),
      session_id: "session-state",
      path: "src/lib.rs",
      decision: "revert",
      review: {
        session_id: "session-state",
        files: [
          {
            path: "src/lib.rs",
            unified_diff: "",
            status: "reverted",
            truncated: false,
            unrestorable_reason: null,
            original_hash: "old-hash",
            current_hash: "old-hash",
          },
        ],
      },
    })
    expect(state.review?.files[0]?.status).toBe("reverted")

    state = reduce(state, {
      type: "sessions_search_ready",
      meta: replyMeta("search-1"),
      query: "rott",
      sessions: [
        {
          session_id: "session-state",
          workspace_name: "Rottweiler",
          model: "fast",
          driver_client_id: null,
          shell_active: false,
        },
      ],
      truncated: true,
    })
    expect(state.sessions).toEqual([
      {
        sessionId: "session-state",
        workspaceName: "Rottweiler",
        model: "fast",
        driverClientId: null,
        shellActive: false,
      },
    ])
    expect(state.sessionSearch).toEqual({ query: "rott", truncated: true })
    expect(state.lastSequence).toBeNull()
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
