import { describe, expect, test } from "bun:test"
import {
  createInitialState,
  MAX_RETAINED_TOOL_PROJECTIONS
} from "../../src/state"
import { isWireEngineEvent } from "../../src/transport"
import { meta, metaAt, reduce } from "./fixtures"

describe("state tools", () => {

  test("projects exact durable tool and turn timing from event timestamps", () => {
    let state = reduce(createInitialState(), {
      type: "turn_started",
      meta: metaAt("1", "2026-01-01T12:00:00.000Z"),
      turn_id: "timed-turn",
    })
    state = reduce(state, {
      type: "tool_call_started",
      meta: metaAt("2", "2026-01-01T12:00:00.000Z"),
      turn_id: "timed-turn",
      tool_call_id: "timed-tool",
      invocation_id: "timed-tool",
      name: "bash",
      args: { command: "bun test" },
      call_index: 0,
    })
    state = reduce(state, {
      type: "tool_output_delta",
      meta: metaAt("3", "2026-01-01T12:00:03.000Z"),
      turn_id: "timed-turn",
      tool_call_id: "timed-tool",
      invocation_id: "timed-tool",
      stream: "stdout",
      chunk: "running",
    })
    state = reduce(state, {
      type: "tool_call_finished", presentation: null,
      meta: metaAt("4", "2026-01-01T12:00:05.000Z"),
      turn_id: "timed-turn",
      tool_call_id: "timed-tool",
      invocation_id: "timed-tool",
      output: { type: "text", text: "done" },
      is_error: false,
      call_index: 0,
    })
    state = reduce(state, {
      type: "turn_finished",
      meta: metaAt("5", "2026-01-01T12:00:05.000Z"),
      turn_id: "timed-turn",
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

    expect(state.tools["timed-tool"]?.timing).toEqual({
      kind: "closed",
      startedAtMs: Date.parse("2026-01-01T12:00:00.000Z"),
      finishedAtMs: Date.parse("2026-01-01T12:00:05.000Z"),
    })
    expect(state.turns["timed-turn"]?.timing).toEqual({
      kind: "closed",
      startedAtMs: Date.parse("2026-01-01T12:00:00.000Z"),
      finishedAtMs: Date.parse("2026-01-01T12:00:05.000Z"),
    })
  })

  test("keeps late tool observations unknown and records a late finish without a start", () => {
    let state = reduce(createInitialState(), {
      type: "tool_approval_needed",
      meta: metaAt("1", "2026-01-01T12:00:03.000Z"),
      turn_id: "late-turn",
      tool_call_id: "late-tool",
      invocation_id: "late-tool",
      name: "edit",
      args: { path: "src/app.ts" },
      capabilities: ["write_filesystem"],
      rationale: "Apply the requested change",
    })
    state = reduce(state, {
      type: "tool_output_delta",
      meta: metaAt("2", "2026-01-01T12:00:04.000Z"),
      turn_id: "late-turn",
      tool_call_id: "late-tool",
      invocation_id: "late-tool",
      stream: "stderr",
      chunk: "waiting",
    })
    expect(state.tools["late-tool"]?.timing).toEqual({ kind: "unknown" })

    state = reduce(state, {
      type: "tool_call_finished", presentation: null,
      meta: metaAt("3", "2026-01-01T12:00:05.000Z"),
      turn_id: "late-turn",
      tool_call_id: "late-tool",
      invocation_id: "late-tool",
      output: { type: "text", text: "permission denied for tool edit" },
      is_error: true,
      call_index: 0,
    })
    expect(state.tools["late-tool"]?.timing).toEqual({
      kind: "closed",
      startedAtMs: null,
      finishedAtMs: Date.parse("2026-01-01T12:00:05.000Z"),
    })
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
        invocation_id: toolCallId,
        name: "read",
        args: { path: `${toolCallId}.txt` },
        call_index: 0,
      })
      state = reduce(state, {
        type: "tool_call_finished", presentation: null,
        meta: meta(`${index * 2 + 2}`),
        turn_id: `${index + 1}`,
        tool_call_id: toolCallId,
        invocation_id: toolCallId,
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
      invocation_id: activeToolId,
      name: "bash",
      args: { command: "sleep 1" },
      call_index: 0,
    })
    expect(state.tools[activeToolId]?.status).toBe("running")
    expect(Object.keys(state.tools)).toHaveLength(MAX_RETAINED_TOOL_PROJECTIONS)
  })

  test("retains tool activity when attach or replay begins after the start event", () => {
    let state = reduce(createInitialState(), {
      type: "tool_approval_needed",
      meta: meta("1"),
      turn_id: "8",
      tool_call_id: "late-glob",
      invocation_id: "late-glob",
      name: "glob",
      args: { pattern: "**/*.rs", path: "." },
      capabilities: ["read_filesystem"],
      rationale: "Inspect workspace files",
    })
    expect(state.streamingTail?.toolInvocationIds).toEqual(["late-glob"])
    expect(state.tools["late-glob"]?.status).toBe("awaiting_approval")

    state = reduce(state, {
      type: "tool_output_delta",
      meta: meta("2"),
      turn_id: "8",
      tool_call_id: "late-glob",
      invocation_id: "late-glob",
      stream: "stdout",
      chunk: "src/lib.rs",
    })
    state = reduce(state, {
      type: "tool_call_finished", presentation: null,
      meta: meta("3"),
      turn_id: "8",
      tool_call_id: "late-glob",
      invocation_id: "late-glob",
      output: { type: "text", text: "src/lib.rs" },
      is_error: false,
      call_index: 0,
    })
    expect(state.streamingTail?.toolInvocationIds).toEqual(["late-glob"])
    expect(state.tools["late-glob"]?.chunks.count).toBe(0)
    expect(state.tools["late-glob"]?.display?.details).toBe("src/lib.rs")
    expect(state.tools["late-glob"]?.status).toBe("finished")
  })

  test("prunes future numeric turn and tool projections when conversation rewinds", () => {
    let state = reduce(createInitialState(), {
      type: "turn_started",
      meta: meta("1"),
      turn_id: "1",
    })
    state = reduce(state, {
      type: "tool_call_started",
      meta: meta("2"),
      turn_id: "1",
      tool_call_id: "tool-1",
      invocation_id: "tool-1",
      name: "read",
      args: { path: "one.txt" },
      call_index: 0,
    })
    state = reduce(state, {
      type: "turn_started",
      meta: meta("3"),
      turn_id: "2",
    })
    state = reduce(state, {
      type: "tool_call_started",
      meta: meta("4"),
      turn_id: "2",
      tool_call_id: "tool-2",
      invocation_id: "tool-2",
      name: "read",
      args: { path: "two.txt" },
      call_index: 0,
    })
    state = {
      ...state,
      turns: {
        ...state.turns,
        "opaque-turn": {
          ...state.turns["2"]!,
          turnId: "opaque-turn",
        },
      },
      tools: {
        ...state.tools,
        "opaque-tool": {
          ...state.tools["tool-2"]!,
          toolCallId: "opaque-tool",
          invocationId: "opaque-tool",
          turnId: "opaque-turn",
        },
      },
    }

    state = reduce(state, {
      type: "conversation_rewound",
      meta: meta("5"),
      to_agent_turn: "1",
      operation_id: "rewind-projections",
      unrestorable_paths: [],
    })

    expect(Object.keys(state.turns)).toEqual(["1", "opaque-turn"])
    expect(Object.keys(state.tools)).toEqual(["tool-1", "opaque-tool"])
    expect(state.turns["2"]).toBeUndefined()
    expect(state.tools["tool-2"]).toBeUndefined()
  })

  test("provider correlation reuse retains independent invocation state and routes late events exactly", () => {
    let state = createInitialState()
    for (const [sequence, invocation] of [["1", "first"], ["2", "second"]] as const) {
      state = reduce(state, { type: "tool_call_started", meta: meta(sequence), turn_id: "1", tool_call_id: "reused", invocation_id: invocation, name: "read", args: {}, call_index: 0 })
    }
    const second = state.tools.second
    state = reduce(state, { type: "tool_output_delta", meta: meta("3"), turn_id: "1", tool_call_id: "reused", invocation_id: "first", stream: "stdout", chunk: "first output" })
    expect(state.tools.first?.chunks.read().plain).toBe("first output")
    state = reduce(state, { type: "tool_call_finished", presentation: null, meta: meta("4"), turn_id: "1", tool_call_id: "reused", invocation_id: "first", output: { type: "text", text: "first result" }, is_error: false, call_index: 0 })
    expect(Object.keys(state.tools)).toEqual(["first", "second"])
    expect(state.tools.reused).toBeUndefined()
    expect(state.tools.first?.display?.details).toBe("first result")
    expect(state.tools.second).toBe(second)
    expect(state.streamingTail?.toolInvocationIds).toEqual(["first", "second"])
    const before = state.tools
    state = reduce(state, { type: "tool_output_delta", meta: meta("5"), turn_id: "1", tool_call_id: "foreign-correlation", invocation_id: "second", stream: "stdout", chunk: "incorrect identity" })
    expect(state.tools).toBe(before)
    const cursor = state.lastSequence
    state = reduce(state, { type: "tool_progress", session_id: "session-state", turn_id: "1", tool_call_id: "reused", invocation_id: "first", progress: { message: "late" } })
    expect(state.lastSequence).toBe(cursor)
    expect(state.tools.second).toBe(second)
  })

  test("progress wire validation enforces plain Unicode text and count relationships", () => {
    const event = { type: "tool_progress", session_id: "session-state", turn_id: "1", tool_call_id: "call", invocation_id: "invocation", progress: { message: "🐕".repeat(256), amount: { completed: 1, total: 2 } } }
    expect(isWireEngineEvent(event)).toBe(true)
    for (const progress of [{ message: "bad\n" }, { message: "🐕".repeat(257) }, { message: "work", amount: { completed: 2, total: 1 } }]) {
      expect(isWireEngineEvent({ ...event, progress })).toBe(false)
    }
  })

  test("retains an inline mutation diff without requiring an approval event", () => {
    let state = reduce(createInitialState(), {
      type: "tool_call_started",
      meta: meta("1"),
      turn_id: "9",
      tool_call_id: "yolo-write",
      invocation_id: "yolo-write",
      name: "write",
      args: { path: "src/main.rs", content: "new" },
      call_index: 0,
    })
    state = reduce(state, {
      type: "tool_diff_ready",
      meta: meta("2"),
      turn_id: "9",
      tool_call_id: "yolo-write",
      invocation_id: "yolo-write",
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
      type: "tool_call_finished", presentation: null,
      meta: meta("3"),
      turn_id: "9",
      tool_call_id: "yolo-write",
      invocation_id: "yolo-write",
      output: { type: "text", text: "updated src/main.rs" },
      is_error: false,
      call_index: 0,
    })

    expect(state.tools["yolo-write"]?.status).toBe("finished")
    expect(state.tools["yolo-write"]?.diff?.path).toBe("src/main.rs")
    expect(state.tools["yolo-write"]?.diff?.unified_diff).toContain("+new")
    expect(state.streamingTail?.toolInvocationIds).toEqual(["yolo-write"])
  })
})
