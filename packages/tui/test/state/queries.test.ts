import { describe, expect, test } from "bun:test"
import {
  PROTOCOL_VERSION,
  type ContextSnapshot,
  type CostSnapshot,
  type EngineEvent
} from "../../src/protocol"
import {
  createInitialState,
  MAX_SHELL_OUTPUT_LINES
} from "../../src/state"
import { toolOutputBuffer } from "../../src/state/display-buffer"
import { meta, reduce } from "./fixtures"

describe("state queries", () => {

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

  test("bounds command acknowledgement history to the newest requests", () => {
    let state = createInitialState()
    for (let index = 0; index < 300; index += 1) {
      state = reduce(state, {
        type: "command_acknowledged",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          client_id: "client",
          request_id: `request-${index}`,
          emitted_at: "2026-01-01T00:00:00Z",
        },
        session_id: "session-state",
        outcome: { type: "accepted" },
      })
    }
    expect(Object.keys(state.commandAcks)).toHaveLength(256)
    expect(state.commandAcks["request-43"]).toBeUndefined()
    expect(state.commandAcks["request-44"]).toBeDefined()
    expect(state.commandAcks["request-299"]).toBeDefined()
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
        { title: "Fixture",
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
        title: "Fixture",
        workspaceName: "Rottweiler",
        model: "fast",
        driverClientId: null,
        shellActive: false,
      },
    ])
    expect(state.sessionSearch).toEqual({ query: "rott", truncated: true })
    expect(state.lastSequence).toBeNull()
  })

  test("projects typed permission inventories as connection-scoped state", () => {
    const permissions = {
      default: "ask" as const,
      effective_rules: [{ id: "effective:one", pattern: "bash(rm *)", action: "deny" as const }],
      project_rules: [],
      session_rules: [{ id: "session:one", pattern: "bash(cargo test*)", action: "ask" as const }],
      approvals: [{
        id: "session:opaque",
        scope: "session" as const,
        tool_name: "bash",
        summary: "exact-invocation=hidden",
      }],
      truncated: false,
    }
    const state = reduce(createInitialState(), {
      type: "permissions_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client-state",
        request_id: "permission-request",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-1",
      permissions,
    })
    expect(state.permissions).toEqual(permissions)
    expect(state.commandAcks["permission-request"]).toMatchObject({
      responseType: "permissions_listed",
      sessionId: "session-1",
    })
  })

  test("projects turns, tools, questions, snapshots, mode, model, and shell state", () => {
    const context = {
      turn_id: "4",
      stable_prefix_hash: "stable",
      used_tokens: "10",
      usable_tokens: "100",
      reserved_tokens: "20",
      context_window_known: true,
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
      session_subscription_tokens: "0",
      daily_cost_micros_usd: "5",
      daily_ai_credit_micros: "0",
      daily_subscription_tokens: "0",
      trailing_minute_cost_micros_usd: "5",
      trailing_minute_ai_credit_micros: "0",
      trailing_minute_subscription_tokens: "0",
      cache_hit_basis_points: 0,
      session_cost_cap_micros_usd: null,
      daily_cost_cap_micros_usd: null,
      session_ai_credit_cap_micros: null,
      daily_ai_credit_cap_micros: null,
      session_token_cap: null,
      daily_token_cap: null,
      spend_rate_alarm_micros_usd_per_minute: null,
      ai_credit_rate_alarm_micros_per_minute: null,
      token_rate_alarm_per_minute: null,
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
      { type: "mode_changed", meta: meta("1"), mode: "execute", definition_fingerprint: "fixture" },
      { type: "model_changed", meta: meta("2"), model: "fast" },
      { type: "turn_started", meta: meta("3"), turn_id: "4" },
      {
        type: "tool_call_started",
        meta: meta("4"),
        turn_id: "4",
        tool_call_id: "tool-1",
        invocation_id: "tool-1",
        name: "read",
        args: { path: "README.md" },
        call_index: 0,
      },
      {
        type: "tool_approval_needed",
        meta: meta("5"),
        turn_id: "4",
        tool_call_id: "tool-1",
        invocation_id: "tool-1",
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
        invocation_id: "tool-1",
        stream: "stdout",
        chunk: "live",
      },
      {
        type: "tool_call_finished",
        meta: meta("7"),
        turn_id: "4",
        tool_call_id: "tool-1",
        invocation_id: "tool-1",
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
      chunks: toolOutputBuffer([]),
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

  test("retains one bounded display-safe foreground shell card from start through completion", () => {
    let state = reduce(createInitialState(), {
      type: "user_shell_state_changed",
      meta: meta("1"),
      shell_id: "shell-visible",
      command: "printf '\\e[31mhello\\e[0m'",
      active: true,
    })
    expect(state.latestShell).toMatchObject({ shellId: "shell-visible", active: true, status: null })

    const noisyOutput = ["\u001b[31mred\u001b[0m\u0000", ...Array.from(
      { length: MAX_SHELL_OUTPUT_LINES + 20 },
      (_, index) => `line ${index}`,
    )].join("\n")
    state = reduce(state, {
      type: "user_shell_state_changed",
      meta: meta("2"),
      shell_id: "shell-visible",
      active: false,
      status: 7,
      captured_output: noisyOutput,
    })

    expect(state.latestShell).toMatchObject({
      active: false,
      status: 7,
      outputTruncated: true,
    })
    expect(state.latestShell?.capturedOutput).not.toContain("\u001b")
    expect(state.latestShell?.capturedOutput).not.toContain("\u0000")
    expect(state.latestShell?.capturedOutput).toContain("more lines")
    expect(state.shell.capturedOutput).toBe(state.latestShell?.capturedOutput ?? null)
  })

  test("projects queued-message removal and clear broadcasts by stable position", () => {
    let state = reduce(createInitialState(), {
      type: "message_queued",
      meta: meta("1"),
      position: "1",
      content: "first",
      attachments: [],
    })
    state = reduce(state, {
      type: "message_queued",
      meta: meta("2"),
      position: "2",
      content: "second",
      attachments: [],
    })
    state = reduce(state, {
      type: "queued_message_removed",
      meta: meta("3"),
      position: "1",
    })
    expect(state.queuedMessages).toEqual([{ position: "2", content: "second" }])

    state = reduce(state, {
      type: "queued_messages_cleared",
      meta: meta("4"),
    })
    expect(state.queuedMessages).toEqual([])
  })
})
