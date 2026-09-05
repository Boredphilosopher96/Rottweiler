import { expect, test } from "bun:test"
import { MAX_PENDING_TOOL_INVOCATIONS } from "../../../../protocol/types"
import { createInitialState, MAX_RETAINED_TOOL_PROJECTIONS, type RottweilerState } from "../../src/state"
import { meta, reduce } from "./fixtures"

function start(state: RottweilerState, sequence: number, invocation: string): RottweilerState {
  return reduce(state, {
    type: "tool_call_started", meta: meta(String(sequence)), turn_id: "turn",
    tool_call_id: "reused-provider-id", invocation_id: invocation, name: "bash", args: {}, call_index: 0,
  })
}

function finish(state: RottweilerState, sequence: number, invocation: string): RottweilerState {
  return reduce(state, {
    type: "tool_call_finished", meta: meta(String(sequence)), turn_id: "turn",
    tool_call_id: "reused-provider-id", invocation_id: invocation, call_index: 0,
    output: { type: "text", text: "done" }, is_error: false, presentation: null,
  })
}

test("one long turn retires completed tail identities while preserving unresolved work and old snapshots", () => {
  let state = start(createInitialState(), 1, "pending")
  const original = state
  let sequence = 2
  for (let iteration = 0; iteration < 1_000; iteration++) {
    const invocation = `invocation-${iteration}`
    state = start(state, sequence++, invocation)
    state = finish(state, sequence++, invocation)
    expect(state.streamingTail?.toolInvocationIds).toEqual(Object.keys(state.tools))
    expect(state.streamingTail!.toolInvocationIds.length).toBeLessThanOrEqual(MAX_RETAINED_TOOL_PROJECTIONS)
    expect(state.tools.pending?.status).toBe("running")
  }
  expect(original.streamingTail?.toolInvocationIds).toEqual(["pending"])
  expect(Object.keys(original.tools)).toEqual(["pending"])
  expect(state.tools["invocation-0"]).toBeUndefined()
  const settled = finish(state, sequence, "pending")
  expect(settled.streamingTail?.toolInvocationIds).toEqual(state.streamingTail?.toolInvocationIds)
})

test("the admitted pending batch retains every invocation until completion", () => {
  let state = createInitialState()
  for (let i = 0; i < MAX_PENDING_TOOL_INVOCATIONS; i++) state = start(state, i + 1, `pending-${i}`)
  expect(state.streamingTail?.toolInvocationIds.length).toBe(MAX_PENDING_TOOL_INVOCATIONS)
  expect(Object.values(state.tools).every((tool) => tool.status === "running")).toBe(true)
  const pending = state
  for (let i = 0; i < MAX_PENDING_TOOL_INVOCATIONS; i++) {
    state = finish(state, MAX_PENDING_TOOL_INVOCATIONS + i + 1, `pending-${i}`)
    expect(state.streamingTail?.toolInvocationIds).toEqual(Object.keys(state.tools))
  }
  expect(state.streamingTail?.toolInvocationIds.length).toBe(MAX_RETAINED_TOOL_PROJECTIONS)
  expect(pending.streamingTail?.toolInvocationIds.length).toBe(MAX_PENDING_TOOL_INVOCATIONS)
})
