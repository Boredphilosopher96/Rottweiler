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
  MAX_COMPACTION_STREAM_BYTES,
  MAX_RETAINED_TOOL_PROJECTIONS,
  MAX_RETAINED_TRANSCRIPT_ENTRIES,
  MAX_RETAINED_TURN_PROJECTIONS,
  MAX_SHELL_OUTPUT_LINES,
  MAX_SUBAGENT_TASK_BYTES,
  MAX_TERMINAL_SUBAGENT_HISTORY,
  MAX_TODO_CONTENT_BYTES,
  reduceRottweilerState,
  transportConnected,
  transportDisconnected,
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
  test("bounds retained transcript and completed turn history", () => {
    let state = createInitialState()
    const total = MAX_RETAINED_TRANSCRIPT_ENTRIES + 8
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

    expect(state.transcript).toHaveLength(MAX_RETAINED_TRANSCRIPT_ENTRIES)
    expect(state.transcript[0]?.agentTurn).toBe("9")
    expect(Object.keys(state.turns)).toHaveLength(MAX_RETAINED_TURN_PROJECTIONS)
    expect(state.turns["1"]).toBeUndefined()
    expect(state.turns[`${total}`]?.status).toBe("completed")
  })

  test("bounds completed tool projections while preserving active work", () => {
    let state = createInitialState()
    for (let index = 0; index < MAX_RETAINED_TOOL_PROJECTIONS + 4; index += 1) {
      const toolCallId = `tool-${index}`
      state = reduce(state, {
        type: "tool_call_started",
        meta: meta(`${index * 2 + 1}`),
        turn_id: `${index + 1}`,
        tool_call_id: toolCallId,
        name: "read",
        args: { path: `${toolCallId}.txt` },
        call_index: 0,
      })
      state = reduce(state, {
        type: "tool_call_finished",
        meta: meta(`${index * 2 + 2}`),
        turn_id: `${index + 1}`,
        tool_call_id: toolCallId,
        output: { type: "text", text: "done" },
        is_error: false,
        call_index: 0,
      })
    }

    expect(Object.keys(state.tools)).toHaveLength(MAX_RETAINED_TOOL_PROJECTIONS)
    expect(state.tools["tool-0"]).toBeUndefined()
    expect(state.tools[`tool-${MAX_RETAINED_TOOL_PROJECTIONS + 3}`]?.status).toBe("finished")

    const activeToolId = "tool-active"
    state = reduce(state, {
      type: "tool_call_started",
      meta: meta(`${(MAX_RETAINED_TOOL_PROJECTIONS + 4) * 2 + 1}`),
      turn_id: "100",
      tool_call_id: activeToolId,
      name: "bash",
      args: { command: "sleep 1" },
      call_index: 0,
    })
    expect(state.tools[activeToolId]?.status).toBe("running")
    expect(Object.keys(state.tools)).toHaveLength(MAX_RETAINED_TOOL_PROJECTIONS)
  })

  test("projects only the live runtime services returned by the host", () => {
    const state = reduce(createInitialState(), {
      type: "runtime_services_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client-services",
        request_id: "request-services",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-state",
      services: [
        { kind: "lsp", name: "rust-analyzer" },
        { kind: "linter", name: "clippy-driver" },
      ],
    })

    expect(state.runtimeServices).toEqual([
      { kind: "lsp", name: "rust-analyzer" },
      { kind: "linter", name: "clippy-driver" },
    ])
    expect(state.commandAcks["request-services"]?.responseType).toBe("runtime_services_listed")
  })

  test("drops connection-scoped provider auth challenges on disconnect", () => {
    const pending = reduce(createInitialState(), {
      type: "provider_auth_started",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client-auth",
        request_id: "request-auth",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-state",
      provider: "github_copilot",
      attempt_id: "attempt-auth",
      challenge: {
        kind: "device_code",
        verification_uri: "https://github.com/login/device",
        user_code: "ABCD-EFGH",
        expires_in_seconds: 900,
        poll_interval_seconds: 5,
      },
      warnings: [],
    })
    expect(pending.providerAuth.pending?.attemptId).toBe("attempt-auth")
    const disconnected = reduceRottweilerState(
      pending,
      transportDisconnected(1, "fixture disconnect"),
    )
    expect(disconnected.providerAuth.pending).toBeNull()
  })

  test("retains command catalog truncation so the UI cannot imply completeness", () => {
    const state = reduce(createInitialState(), {
      type: "command_descriptors_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client",
        request_id: "commands",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session",
      commands: [{ name: "fixture", description: "Fixture", usage: "" }],
      truncated: true,
    })
    expect(state.commandsTruncated).toBeTrue()
  })

  test("projects a typed custom mode catalog and its completeness", () => {
    const state = reduce(createInitialState(), {
      type: "modes_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client",
        request_id: "modes",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session",
      modes: [
        { id: "execute", description: "Make changes", current: false },
        { id: "audit", description: "Inspect controls", current: true },
      ],
      truncated: true,
    })
    expect(state.modes).toEqual([
      { id: "execute", description: "Make changes", current: false },
      { id: "audit", description: "Inspect controls", current: true },
    ])
    expect(state.modesTruncated).toBeTrue()
    expect(state.mode).toBe("audit")
  })

  test("defaults missing providers from older model-list events", () => {
    const state = reduce(createInitialState(), {
      type: "models_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client-old",
        request_id: "request-old",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      models: [
        {
          alias: "fast",
          capabilities: {
            tool_calling: true,
            vision: false,
            thinking: false,
            cache_behavior: "none",
          },
        },
      ],
    })
    expect(state.models).toEqual([
      {
        alias: "fast",
        providers: [],
        vision: false,
        thinking: false,
        toolCalling: true,
      },
    ])
  })

  test("projects the unique concrete current model before the first turn", () => {
    const state = reduce(createInitialState(), {
      type: "models_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client-current",
        request_id: "request-current",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      models: [
        {
          alias: "openai/gpt-5-mini",
          id: "openai/gpt-5-mini",
          display_name: "GPT-5 mini",
          provider: "openai",
          providers: ["openai"],
          aliases: ["fast"],
          current: true,
          available: true,
          capabilities: {
            tool_calling: true,
            vision: true,
            thinking: true,
            cache_behavior: "none",
          },
        },
      ],
      aliases: [{ alias: "fast", candidates: ["openai/gpt-5-mini"], current: true }],
      providers: [],
    })
    expect(state.model).toBe("openai/gpt-5-mini")
    expect(state.provider).toBe("openai")
  })

  test("projects the active session model when no model is set", () => {
    const withDriver = reduce(createInitialState(), {
      type: "driver_changed",
      meta: meta("1"),
      driver_client_id: "active-client",
    })
    const state = reduce(withDriver, {
      type: "sessions_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client",
        request_id: "sessions",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      sessions: [
        {
          session_id: "other-session",
          workspace_name: "Rottweiler",
          model: "other-model",
          driver_client_id: "other-client",
          shell_active: false,
        },
        {
          session_id: "active-session",
          workspace_name: "Rottweiler",
          model: "active-model",
          driver_client_id: "active-client",
          shell_active: false,
        },
      ],
    })
    expect(state.model).toBe("active-model")
  })

  test("does not infer an active session without a driver client", () => {
    const state = reduce(createInitialState(), {
      type: "sessions_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client",
        request_id: "sessions",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      sessions: [
        {
          session_id: "session",
          workspace_name: "Rottweiler",
          model: "model",
          driver_client_id: null,
          shell_active: false,
        },
      ],
    })
    expect(state.model).toBeNull()
  })

  test("projects session title updates into the matching sessions-picker row", () => {
    const listed = reduce(createInitialState(), {
      type: "sessions_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client",
        request_id: "sessions",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      sessions: [
        {
          session_id: "session-state",
          title: "Old title",
          workspace_name: "Rottweiler",
          model: "fast",
          driver_client_id: null,
          shell_active: false,
        },
        {
          session_id: "other-session",
          title: "Keep me",
          workspace_name: "Other",
          model: "fast",
          driver_client_id: null,
          shell_active: false,
        },
      ],
    })
    const renamed = reduce(listed, {
      type: "session_title_updated",
      meta: meta("0"),
      title: "Auth refactor",
      usage: null,
      cost: null,
    })

    expect(renamed.sessions.map((session) => [session.sessionId, session.title])).toEqual([
      ["session-state", "Auth refactor"],
      ["other-session", "Keep me"],
    ])
  })

  test("model catalog refresh does not overwrite a newer durable model event", () => {
    const durable = reduce(createInitialState(), {
      type: "model_changed",
      meta: meta("1"),
      model: "anthropic/claude-sonnet-4-5",
      provider: "anthropic",
    })
    const refreshed = reduce(durable, {
      type: "models_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client-current",
        request_id: "request-current",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      models: [
        {
          alias: "openai/gpt-5-mini",
          id: "openai/gpt-5-mini",
          display_name: "GPT-5 mini",
          provider: "openai",
          providers: ["openai"],
          aliases: ["fast"],
          current: true,
          available: true,
          capabilities: {
            tool_calling: true,
            vision: true,
            thinking: true,
            cache_behavior: "none",
          },
        },
      ],
      aliases: [],
      providers: [],
    })
    expect(refreshed.model).toBe("anthropic/claude-sonnet-4-5")
    expect(refreshed.provider).toBe("anthropic")
  })

  test("projects only bounded successful todo tool snapshots", () => {
    let state = reduce(createInitialState(), {
      type: "tool_call_started",
      meta: meta("1"),
      turn_id: "2",
      tool_call_id: "todo-valid",
      name: "todo",
      args: { action: "replace" },
      call_index: 0,
    })
    state = reduce(state, {
      type: "tool_call_finished",
      meta: meta("2"),
      turn_id: "2",
      tool_call_id: "todo-valid",
      output: {
        type: "mixed",
        parts: [
          { type: "text", text: "[InProgress] audit: Audit TUI" },
          {
            type: "structured",
            value: {
              data: {
                items: [
                  { id: "audit", content: "Audit TUI", status: "in_progress" },
                  { id: "tests", content: "Add tests", status: "pending" },
                ],
                count: 2,
              },
              truncated: false,
            },
          },
        ],
      },
      is_error: false,
      call_index: 0,
    })
    expect(state.todos).toEqual([
      { id: "audit", content: "Audit TUI", status: "in_progress" },
      { id: "tests", content: "Add tests", status: "pending" },
    ])

    state = reduce(state, {
      type: "tool_call_started",
      meta: meta("3"),
      turn_id: "3",
      tool_call_id: "todo-unbounded",
      name: "todo",
      args: { action: "replace" },
      call_index: 0,
    })
    state = reduce(state, {
      type: "tool_call_finished",
      meta: meta("4"),
      turn_id: "3",
      tool_call_id: "todo-unbounded",
      output: {
        type: "structured",
        value: {
          items: [{ id: "huge", content: "x".repeat(MAX_TODO_CONTENT_BYTES + 1), status: "pending" }],
          count: 1,
        },
      },
      is_error: false,
      call_index: 0,
    })
    expect(state.todos.map((todo) => todo.id)).toEqual(["audit", "tests"])

    state = reduce(state, {
      type: "tool_call_started",
      meta: meta("5"),
      turn_id: "4",
      tool_call_id: "todo-malformed",
      name: "todo",
      args: { action: "replace" },
      call_index: 0,
    })
    state = reduce(state, {
      type: "tool_call_finished",
      meta: meta("6"),
      turn_id: "4",
      tool_call_id: "todo-malformed",
      output: {
        type: "structured",
        value: {
          items: [
            { id: "duplicate", content: "one", status: "pending" },
            { id: "duplicate", content: "two", status: "unknown" },
          ],
          count: 2,
        },
      },
      is_error: false,
      call_index: 0,
    })
    expect(state.todos.map((todo) => todo.id)).toEqual(["audit", "tests"])
  })

  test("retains tool activity when attach or replay begins after the start event", () => {
    let state = reduce(createInitialState(), {
      type: "tool_approval_needed",
      meta: meta("1"),
      turn_id: "8",
      tool_call_id: "late-glob",
      name: "glob",
      args: { pattern: "**/*.rs", path: "." },
      capabilities: ["read_filesystem"],
      rationale: "Inspect workspace files",
    })
    expect(state.streamingTail?.toolCallIds).toEqual(["late-glob"])
    expect(state.tools["late-glob"]?.status).toBe("awaiting_approval")

    state = reduce(state, {
      type: "tool_output_delta",
      meta: meta("2"),
      turn_id: "8",
      tool_call_id: "late-glob",
      stream: "stdout",
      chunk: "src/lib.rs",
    })
    state = reduce(state, {
      type: "tool_call_finished",
      meta: meta("3"),
      turn_id: "8",
      tool_call_id: "late-glob",
      output: { type: "text", text: "src/lib.rs" },
      is_error: false,
      call_index: 0,
    })
    expect(state.streamingTail?.toolCallIds).toEqual(["late-glob"])
    expect(state.tools["late-glob"]?.chunks).toEqual([
      { stream: "stdout", chunk: "src/lib.rs" },
    ])
    expect(state.tools["late-glob"]?.status).toBe("finished")
  })

  test("retains an inline mutation diff without requiring an approval event", () => {
    let state = reduce(createInitialState(), {
      type: "tool_call_started",
      meta: meta("1"),
      turn_id: "9",
      tool_call_id: "yolo-write",
      name: "write",
      args: { path: "src/main.rs", content: "new" },
      call_index: 0,
    })
    state = reduce(state, {
      type: "tool_diff_ready",
      meta: meta("2"),
      turn_id: "9",
      tool_call_id: "yolo-write",
      diff: {
        proposal_id: "proposal-yolo",
        path: "src/main.rs",
        unified_diff: "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-old\n+new\n",
        arguments_hash: "args",
        base_hash: "base",
        diff_hash: "diff",
        truncated: false,
      },
    })
    state = reduce(state, {
      type: "tool_call_finished",
      meta: meta("3"),
      turn_id: "9",
      tool_call_id: "yolo-write",
      output: { type: "text", text: "updated src/main.rs" },
      is_error: false,
      call_index: 0,
    })

    expect(state.tools["yolo-write"]?.status).toBe("finished")
    expect(state.tools["yolo-write"]?.diff?.path).toBe("src/main.rs")
    expect(state.tools["yolo-write"]?.diff?.unified_diff).toContain("+new")
    expect(state.streamingTail?.toolCallIds).toEqual(["yolo-write"])
  })

  test("rederives the latest valid todo snapshot at a rewind boundary", () => {
    let state = createInitialState()
    for (const [sequence, turn, id, content] of [
      ["1", "1", "first", "First task"],
      ["3", "4", "later", "Later task"],
    ] as const) {
      state = reduce(state, {
        type: "tool_call_started",
        meta: meta(sequence),
        turn_id: turn,
        tool_call_id: `todo-${id}`,
        name: "todo",
        args: { action: "replace" },
        call_index: 0,
      })
      state = reduce(state, {
        type: "tool_call_finished",
        meta: meta(String(Number(sequence) + 1)),
        turn_id: turn,
        tool_call_id: `todo-${id}`,
        output: {
          type: "mixed",
          parts: [{
            type: "structured",
            value: { data: { items: [{ id, content, status: "pending" }], count: 1 }, truncated: false },
          }],
        },
        is_error: false,
        call_index: 0,
      })
    }
    expect(state.todos.map((todo) => todo.id)).toEqual(["later"])

    state = reduce(state, {
      type: "conversation_rewound",
      meta: meta("5"),
      to_agent_turn: "1",
      operation_id: "rewind-todos",
      unrestorable_paths: [],
    })
    expect(state.todos).toEqual([{ id: "first", content: "First task", status: "pending" }])
  })

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
      meta: { model: "copilot/gpt-5-mini", synthetic: false, summary: false },
    }
    state = reduce(state, {
      type: "conversation_turn_committed",
      meta: meta("4"),
      agent_turn: "7",
      turn,
    })
    expect(state.transcript).toEqual([{ sequenceId: "4", agentTurn: "7", turn }])
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
    const spawnedAtMs = state.subagents.child?.spawnedAtMs
    expect(spawnedAtMs).toBe(Date.parse("2026-01-01T00:00:00Z"))
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
      spawnedAtMs,
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

  test("projects bounded subagent tool arguments without trusting malformed args", () => {
    let state = reduce(createInitialState(), {
      type: "subagent_spawned",
      meta: meta("1"),
      subagent_id: "child",
      child_session_id: "session-child",
      task: "Inspect tools",
    })
    const spawnedAtMs = state.subagents.child?.spawnedAtMs
    const cases = [
      {
        name: "bash",
        args: { command: "bun test\n  test/state-reducer.test.ts --only" },
        expected: "using tool · bash · bun test test/state-reducer.test.ts --only",
      },
      {
        name: "read",
        args: { path: "/Users/example/Rottweiler/packages/tui/src/components/transcript.ts" },
        expected: "using tool · read · components/transcript.ts",
      },
      {
        name: "write",
        args: { file_path: "packages/tui/src/state/model.ts" },
        expected: "using tool · write · state/model.ts",
      },
      {
        name: "edit",
        args: { filePath: "packages/tui/src/app.ts" },
        expected: "using tool · edit · src/app.ts",
      },
      {
        name: "grep",
        args: { pattern: "subagent_(spawned|finished)" },
        expected: "using tool · grep · subagent_(spawned|finished)",
      },
      {
        name: "glob",
        args: { pattern: "packages/tui/**/*.test.ts" },
        expected: "using tool · glob · packages/tui/**/*.test.ts",
      },
      {
        name: "bash",
        args: ["not", "an", "object"],
        expected: "using tool · bash",
      },
    ] as const
    for (const [index, item] of cases.entries()) {
      state = reduce(state, {
        type: "subagent_progress",
        parent_session_id: "session-state",
        subagent_id: "child",
        child_session_id: "session-child",
        child_sequence: String(index + 1),
        event: { type: "tool_call_started", name: item.name, args: item.args },
      })
      expect(state.subagents.child?.activity).toBe(item.expected)
    }
    state = reduce(state, {
      type: "subagent_progress",
      parent_session_id: "session-state",
      subagent_id: "child",
      child_session_id: "session-child",
      child_sequence: "8",
      event: {
        type: "tool_call_started",
        name: "bash",
        args: { command: `${"x".repeat(100)}\nsecret-second-line` },
      },
    })
    expect(state.subagents.child?.activity?.length ?? 0).toBeLessThanOrEqual(72)
    expect(state.subagents.child?.activity).not.toContain("\n")
    expect(state.subagents.child?.spawnedAtMs).toBe(spawnedAtMs)
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

  test("uses null when a subagent spawn event has an invalid emitted_at timestamp", () => {
    const state = reduce(createInitialState(), {
      type: "subagent_spawned",
      meta: { ...meta("1"), emitted_at: "not-a-timestamp" },
      subagent_id: "child",
      child_session_id: "session-child",
      task: "Inspect timestamp handling",
    })

    expect(state.subagents.child?.spawnedAtMs).toBeNull()
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
        daily_cost_micros_usd: "0",
        daily_ai_credit_micros: "0",
        trailing_minute_cost_micros_usd: "0",
        trailing_minute_ai_credit_micros: "0",
        cache_hit_basis_points: 6700,
        session_cost_cap_micros_usd: null,
        daily_cost_cap_micros_usd: null,
        session_ai_credit_cap_micros: null,
        daily_ai_credit_cap_micros: null,
        spend_rate_alarm_micros_usd_per_minute: null,
        ai_credit_rate_alarm_micros_per_minute: null,
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

  test("preserves typed model-switch context choices for the interaction dock", () => {
    const state = reduce(createInitialState(), {
      type: "question_asked",
      meta: meta("1"),
      turn_id: "4",
      question_id: "model-switch-1",
      questions: [{
        id: "model-switch-1",
        prompt: "How should context move to the selected model?",
        response_kind: "select_one",
        model_switch: { model: "openai/gpt-5", provider: "openai" },
        options: [
          {
            value: "pass_summary",
            label: "Pass summary",
            model_context_transfer: "pass_summary",
          },
          {
            value: "pass_full_context",
            label: "Pass full context",
            model_context_transfer: "pass_full_context",
          },
          {
            value: "start_without_context",
            label: "Start without context",
            model_context_transfer: "start_without_context",
          },
        ],
      }],
    })

    expect(state.questions["model-switch-1"]?.questions[0]).toMatchObject({
      model_switch: { model: "openai/gpt-5", provider: "openai" },
      options: [
        { value: "pass_summary", model_context_transfer: "pass_summary" },
        { value: "pass_full_context", model_context_transfer: "pass_full_context" },
        { value: "start_without_context", model_context_transfer: "start_without_context" },
      ],
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
})
