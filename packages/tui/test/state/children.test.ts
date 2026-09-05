import { describe, expect, test } from "bun:test"
import {
  createInitialState,
  MAX_SUBAGENT_TASK_BYTES,
  MAX_TERMINAL_SUBAGENT_HISTORY,
  type RottweilerState
} from "../../src/state"
import { childResult, meta, reduce } from "./fixtures"

describe("state children", () => {

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
    ]
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
      })
      expect(state.subagents[subagentId]?.status).toBe(status)
    }
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
})
