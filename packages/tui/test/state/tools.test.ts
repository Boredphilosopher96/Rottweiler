import { describe, expect, test } from "bun:test"
import {
  createInitialState,
  MAX_RETAINED_TODO_TOOL_PROJECTIONS,
  MAX_RETAINED_TOOL_PROJECTIONS,
  MAX_TODO_CONTENT_BYTES
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
      type: "tool_call_finished",
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
      type: "tool_call_finished",
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
        type: "tool_call_finished",
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

  test("retains bounded todo rewind checkpoints independently from display tools", () => {
    let state = createInitialState()
    for (let turn = 1; turn <= MAX_RETAINED_TODO_TOOL_PROJECTIONS + 1; turn += 1) {
      state = reduce(state, {
        type: "tool_call_started",
        meta: meta(`${turn * 2 - 1}`),
        turn_id: `${turn}`,
        tool_call_id: `todo-${turn}`,
        invocation_id: `todo-${turn}`,
        name: "todo",
        args: { action: "replace" },
        call_index: 0,
      })
      state = reduce(state, {
        type: "tool_call_finished",
        meta: meta(`${turn * 2}`),
        turn_id: `${turn}`,
        tool_call_id: `todo-${turn}`,
        invocation_id: `todo-${turn}`,
        output: {
          type: "structured",
          value: {
            data: {
              items: [{ id: `${turn}`, content: `Task ${turn}`, status: "pending" }],
              count: 1,
            },
            truncated: false,
          },
        },
        is_error: false,
        call_index: 0,
      })
    }

    for (let index = 0; index < MAX_RETAINED_TOOL_PROJECTIONS + 4; index += 1) {
      const toolCallId = `read-${index}`
      const sequence = (MAX_RETAINED_TODO_TOOL_PROJECTIONS + 1) * 2 + index * 2 + 1
      state = reduce(state, {
        type: "tool_call_started",
        meta: meta(`${sequence}`),
        turn_id: `${1_000 + index}`,
        tool_call_id: toolCallId,
        invocation_id: toolCallId,
        name: "read",
        args: { path: `${index}.txt` },
        call_index: 0,
      })
      state = reduce(state, {
        type: "tool_call_finished",
        meta: meta(`${sequence + 1}`),
        turn_id: `${1_000 + index}`,
        tool_call_id: toolCallId,
        invocation_id: toolCallId,
        output: { type: "text", text: "done" },
        is_error: false,
        call_index: 0,
      })
    }

    expect(state.tools["todo-1"]).toBeUndefined()
    expect(state.tools["todo-2"]?.name).toBe("todo")
    expect(Object.values(state.tools).filter((tool) => tool.name === "todo")).toHaveLength(
      MAX_RETAINED_TODO_TOOL_PROJECTIONS,
    )
    expect(Object.values(state.tools).filter((tool) => tool.name === "read")).toHaveLength(
      MAX_RETAINED_TOOL_PROJECTIONS,
    )

    state = reduce(state, {
      type: "conversation_rewound",
      meta: meta(
        `${(MAX_RETAINED_TODO_TOOL_PROJECTIONS + 1) * 2 + (MAX_RETAINED_TOOL_PROJECTIONS + 4) * 2 + 1}`,
      ),
      to_agent_turn: "2",
      operation_id: "rewind-retained-todo",
      unrestorable_paths: [],
    })
    expect(state.todos).toEqual([{ id: "2", content: "Task 2", status: "pending" }])
  })

  test("projects only bounded successful todo tool snapshots", () => {
    let state = reduce(createInitialState(), {
      type: "tool_call_started",
      meta: meta("1"),
      turn_id: "2",
      tool_call_id: "todo-valid",
      invocation_id: "todo-valid",
      name: "todo",
      args: { action: "replace" },
      call_index: 0,
    })
    state = reduce(state, {
      type: "tool_call_finished",
      meta: meta("2"),
      turn_id: "2",
      tool_call_id: "todo-valid",
      invocation_id: "todo-valid",
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
      invocation_id: "todo-unbounded",
      name: "todo",
      args: { action: "replace" },
      call_index: 0,
    })
    state = reduce(state, {
      type: "tool_call_finished",
      meta: meta("4"),
      turn_id: "3",
      tool_call_id: "todo-unbounded",
      invocation_id: "todo-unbounded",
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
      invocation_id: "todo-malformed",
      name: "todo",
      args: { action: "replace" },
      call_index: 0,
    })
    state = reduce(state, {
      type: "tool_call_finished",
      meta: meta("6"),
      turn_id: "4",
      tool_call_id: "todo-malformed",
      invocation_id: "todo-malformed",
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
      invocation_id: "late-glob",
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
      invocation_id: "late-glob",
      stream: "stdout",
      chunk: "src/lib.rs",
    })
    state = reduce(state, {
      type: "tool_call_finished",
      meta: meta("3"),
      turn_id: "8",
      tool_call_id: "late-glob",
      invocation_id: "late-glob",
      output: { type: "text", text: "src/lib.rs" },
      is_error: false,
      call_index: 0,
    })
    expect(state.streamingTail?.toolCallIds).toEqual(["late-glob"])
    expect(state.tools["late-glob"]?.chunks.count).toBe(0)
    expect(state.tools["late-glob"]?.output).toEqual({ type: "text", text: "src/lib.rs" })
    expect(state.tools["late-glob"]?.status).toBe("finished")
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
        invocation_id: `todo-${id}`,
        name: "todo",
        args: { action: "replace" },
        call_index: 0,
      })
      state = reduce(state, {
        type: "tool_call_finished",
        meta: meta(String(Number(sequence) + 1)),
        turn_id: turn,
        tool_call_id: `todo-${id}`,
        invocation_id: `todo-${id}`,
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

  test("reused provider IDs reject late observations from an earlier invocation", () => {
    let state = createInitialState()
    for (const [sequence, invocation] of [["1", "first"], ["2", "second"]] as const) {
      state = reduce(state, { type: "tool_call_started", meta: meta(sequence), turn_id: "1", tool_call_id: "reused", invocation_id: invocation, name: "read", args: {}, call_index: 0 })
    }
    state = reduce(state, { type: "tool_output_delta", meta: meta("3"), turn_id: "1", tool_call_id: "reused", invocation_id: "first", stream: "stdout", chunk: "stale output" })
    state = reduce(state, { type: "tool_call_finished", meta: meta("4"), turn_id: "1", tool_call_id: "reused", invocation_id: "first", output: { type: "text", text: "stale result" }, is_error: false, call_index: 0 })
    expect(state.tools.reused?.invocationId).toBe("second")
    expect(state.tools.reused?.status).toBe("running")
    expect(state.tools.reused?.chunks.count).toBe(0)
    const cursor = state.lastSequence
    state = reduce(state, { type: "tool_progress", session_id: "session-state", turn_id: "1", tool_call_id: "reused", invocation_id: "first", progress: { message: "late" } })
    expect(state.lastSequence).toBe(cursor)
    expect(state.tools.reused?.invocationId).toBe("second")
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
      type: "tool_call_finished",
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
    expect(state.streamingTail?.toolCallIds).toEqual(["yolo-write"])
  })
})
