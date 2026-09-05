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

  test("projects structured command payloads without protocol fields", () => {
    const state = reduce(createInitialState(), {
      type: "command_finished",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-private",
        sequence_id: "4",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      name: "context",
      message: JSON.stringify({
        turn_id: "turn-private",
        stable_prefix_hash: "hash-private",
        data: { paths: ["src/main.rs"], count: 1, approval_state: "approval_required" },
        truncated: false,
      }),
      unrestorable_paths: [],
    })
    const entry = state.transcript.at(-1)
    expect(entry?.turn.blocks).toEqual([])
    expect(entry?.commandResult).toEqual({
      kind: "structured",
      rows: [
        { prefixes: [], label: "paths", value: { kind: "heading" } },
        { prefixes: ["bullet"], label: null, value: { kind: "string", value: "src/main.rs" } },
        { prefixes: [], label: "count", value: { kind: "number", value: 1 } },
        { prefixes: [], label: "approval_state", value: { kind: "string", value: "approval_required" } },
      ],
      omittedRowCount: 0,
    })
    const retained = JSON.stringify(entry?.commandResult)
    expect(retained).not.toContain("turn_id")
    expect(retained).not.toContain("stable_prefix_hash")
    expect(retained).not.toContain("hash-private")
    expect(retained).not.toContain("session-private")
  })

  test("projects built-in command results as bounded semantic content", () => {
    const fixtures = [
      ["help", "/status — Show agent status\n/mode [execute] — Switch mode", {
        kind: "help",
        commands: [
          { usage: "/status", description: "Show agent status" },
          { usage: "/mode [execute]", description: "Switch mode" },
        ],
        omittedCommandCount: 0,
        fallback: null,
      }],
      ["status", "Agent: working\nQueued messages: 2\nMode: execute", {
        kind: "status", agent: "working", mode: "execute", queuedMessages: "2",
      }],
      ["mode", "mode changed to plan", { kind: "mode", mode: "plan", active: false }],
      ["permissions", "Permission mode: yolo\nDefault permission: allow\nConfigured rules:\n- deny · bash(rm *)\nSession rules: none\nRemembered approvals: 1 for this session, 0 for this project", {
        kind: "permissions",
        summary: null,
        mode: "yolo",
        defaultPermission: "allow",
        rememberedApprovals: " 1 for this session, 0 for this project",
        rules: [{ scope: "Project", decision: "deny", target: "bash(rm *)", remembered: false }],
        omittedRuleCount: 0,
      }],
      ["plan", "Ship safely\nKeep state durable.\n1. Update UI\n   Verify: bun test", {
        kind: "plan",
        title: "Ship safely",
        body: { lines: ["Keep state durable.", "1. Update UI", "   Verify: bun test"], omittedLineCount: 0 },
      }],
      ["review", "Session review: 2 changed file(s) · 1 awaiting review\n- src/app.ts · needs review\n- src/lib.rs · accepted", {
        kind: "review",
        summary: "Session review: 2 changed file(s) · 1 awaiting review",
        files: [
          { path: "src/app.ts", status: "needs review", note: "" },
          { path: "src/lib.rs", status: "accepted", note: "" },
        ],
        omittedFileCount: 0,
      }],
      ["trust", "folder trust granted for this workspace", {
        kind: "trust", trust: "trusted", message: "folder trust granted for this workspace",
      }],
      ["mcp", "docs · ready · 4 tools\nsearch · disabled · 0 tools", {
        kind: "mcp",
        updated: false,
        servers: [
          { name: "docs", status: "ready · 4 tools" },
          { name: "search", status: "disabled · 0 tools" },
        ],
        omittedServerCount: 0,
        fallback: null,
      }],
      ["compact", "compaction started", {
        kind: "completion", title: "Compaction started", detail: "compaction started",
      }],
      ["interrupt", "interrupt requested", {
        kind: "completion", title: "Interrupt requested", detail: "interrupt requested",
      }],
      ["rewind", "rewound to turn 4", {
        kind: "completion", title: "Session rewound", detail: "rewound to turn 4",
      }],
      ["add-dir", "added workspace root @root/2", {
        kind: "completion", title: "Workspace updated", detail: "added workspace root @root/2",
      }],
    ] as const
    let state = createInitialState()
    for (const [index, [name, message, expectedProjection]] of fixtures.entries()) {
      state = reduce(state, {
        type: "command_finished",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "session-command-cards",
          sequence_id: String(index + 1),
          emitted_at: "2026-01-01T00:00:00Z",
        },
        name,
        message,
        unrestorable_paths: [],
      })
      const entry = state.transcript.at(-1)
      expect(entry?.turn.blocks).toEqual([])
      expect(entry?.commandResult).toEqual(expectedProjection)
    }

    state = reduce(state, {
      type: "command_finished",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-command-cards",
        sequence_id: String(fixtures.length + 1),
        emitted_at: "2026-01-01T00:00:00Z",
      },
      name: "extension-report",
      message: JSON.stringify({
        data: {
          entries: Array.from({ length: 80 }, (_, index) => ({ label: `entry-${index}` })),
          stable_prefix_hash: "private",
        },
        truncated: false,
      }),
      unrestorable_paths: [],
    })
    const projection = state.transcript.at(-1)?.commandResult
    expect(projection?.kind).toBe("structured")
    if (projection?.kind !== "structured") throw new Error("expected structured projection")
    expect(projection.rows).toHaveLength(24)
    expect(projection.omittedRowCount).toBe(57)
    expect(JSON.stringify(projection)).not.toContain("stable_prefix_hash")
    expect(JSON.stringify(projection)).not.toContain("private")
  })

  test("projects context and cost commands as structured snapshots", () => {
    const ackMeta = (requestId: string) => ({
      protocol_version: PROTOCOL_VERSION,
      client_id: "client-summary",
      request_id: requestId,
      emitted_at: "2026-01-01T00:00:00Z",
    })
    let state = reduce(createInitialState(), {
      type: "context_snapshot_ready",
      meta: ackMeta("context-summary"),
      session_id: "session-summary",
      snapshot: {
        turn_id: "turn-private",
        stable_prefix_hash: "hash-private",
        used_tokens: "63552",
        usable_tokens: "380000",
        reserved_tokens: "20000",
        context_window_known: true,
        cache_breakpoints: [],
        items: [
          {
            item_id: "system:0",
            kind: "system",
            label: "Base instructions",
            source: "built_in",
            machine_local_path: null,
            estimated_tokens: "152",
            state: { pinned: false, evicted: false, summarized: false, pruned: false },
          },
          {
            item_id: "conversation:0",
            kind: "conversation",
            label: "Assistant turn",
            source: "session",
            machine_local_path: null,
            estimated_tokens: "63400",
            state: { pinned: false, evicted: false, summarized: false, pruned: false },
          },
        ],
      },
    })
    state = reduce(state, {
      type: "command_finished",
      meta: { ...meta("1"), session_id: "session-summary" },
      name: "context",
      message: "this long engine copy is intentionally ignored",
      unrestorable_paths: [],
    })
    expect(state.transcript.at(-1)?.turn.blocks).toEqual([])
    expect(state.transcript.at(-1)?.commandResult).toEqual({
      kind: "context",
      usedTokens: "63552",
      usableTokens: "380000",
      reservedTokens: "20000",
      contextWindowKnown: true,
      itemCount: 2,
      groups: [
        { kind: "system", itemCount: 1, estimatedTokens: "152" },
        { kind: "conversation", itemCount: 1, estimatedTokens: "63400" },
      ],
    })

    state = reduce(state, {
      type: "cost_snapshot_ready",
      meta: ackMeta("cost-summary"),
      session_id: "session-summary",
      snapshot: {
        utc_day: "2026-01-01",
        turns: [],
        session_usage: {
          input_tokens: "189823",
          output_tokens: "2771",
          cache_read_tokens: "380096",
          cache_write_tokens: "0",
          reasoning_tokens: "430",
        },
        session_cost_micros_usd: "0",
        session_ai_credit_micros: "0",
        session_subscription_tokens: "0",
        daily_cost_micros_usd: "0",
        daily_ai_credit_micros: "0",
        daily_subscription_tokens: "0",
        trailing_minute_cost_micros_usd: "0",
        trailing_minute_ai_credit_micros: "0",
        trailing_minute_subscription_tokens: "0",
        cache_hit_basis_points: 6700,
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
        session_monetary_accounting_complete: false,
        daily_monetary_accounting_complete: false,
        session_subscription_quota_entries: "1",
        session_cost_unavailable_entries: "0",
        session_non_usd_monetary_entries: "0",
        daily_subscription_quota_entries: "1",
        daily_cost_unavailable_entries: "0",
        daily_non_usd_monetary_entries: "0",
      },
    })
    state = reduce(state, {
      type: "command_finished",
      meta: { ...meta("2"), session_id: "session-summary" },
      name: "cost",
      message: "another long engine copy",
      unrestorable_paths: [],
    })
    expect(state.transcript.at(-1)?.turn.blocks).toEqual([])
    expect(state.transcript.at(-1)?.commandResult).toEqual({
      kind: "cost",
      inputTokens: "189823",
      outputTokens: "2771",
      reasoningTokens: "430",
      cacheReadTokens: "380096",
      cacheHitBasisPoints: 6700,
      subscriptionQuotaEntries: "1",
      costUnavailableEntries: "0",
      monetaryAccountingComplete: false,
      costMicrosUsd: "0",
      accountedTurnCount: 0,
      utcDay: "2026-01-01",
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
    expect(state.transcript).toHaveLength(1)
    expect(state.transcript[0]).toMatchObject({
      sequenceId: "1",
      agentTurn: "shell:shell-visible",
      presentation: "shell_result",
      shell: {
        command: "printf '\\e[31mhello\\e[0m'",
        active: true,
        status: null,
      },
    })

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

    expect(state.transcript).toHaveLength(1)
    expect(state.transcript[0]?.sequenceId).toBe("1")
    expect(state.transcript[0]?.shell).toMatchObject({
      active: false,
      status: 7,
      outputTruncated: true,
    })
    expect(state.transcript[0]?.shell?.capturedOutput).not.toContain("\u001b")
    expect(state.transcript[0]?.shell?.capturedOutput).not.toContain("\u0000")
    expect(state.transcript[0]?.shell?.capturedOutput).toContain("more lines")
    expect(state.shell.capturedOutput).toBe(state.transcript[0]?.shell?.capturedOutput ?? null)
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
    expect(state.protocol.unknownEvents).toBe(0)
  })
})
