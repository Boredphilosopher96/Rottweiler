import { expect, test } from "bun:test"
import { PROTOCOL_VERSION, type ContextSnapshot, type EngineEvent } from "../src/protocol"
import { createInitialState, engineEvent, reduceRottweilerState } from "../src/state"
const apply = (state: ReturnType<typeof createInitialState>, event: EngineEvent) => reduceRottweilerState(state, engineEvent(event), "s")
const snapshot: ContextSnapshot = { through: "1", turn_id: "t", stable_prefix_hash: "captured", used_tokens: "10", usable_tokens: "100", reserved_tokens: "1", context_window_known: true, cache_breakpoints: [], items: [] }
const ready = (value: ContextSnapshot): EngineEvent => ({ type: "context_snapshot_ready", snapshot: value, session_id: "s",
  meta: { protocol_version: PROTOCOL_VERSION, client_id: "c", request_id: "r", emitted_at: "2026-01-01T00:00:00Z" } })
test("live capacity does not relabel or mutate a captured complete context snapshot", () => {
  let state = apply(createInitialState(), ready(snapshot))
  state = apply(state, { type: "context_usage_updated", cache_hit_basis_points: 0, estimated_input_tokens: "50", provider_input_tokens: "50", correction_millionths: "1000000", turn_id: "t", stable_prefix_hash: "live", used_tokens: "50", usable_tokens: "100", reserved_tokens: "2", context_window_known: true,
    meta: { protocol_version: PROTOCOL_VERSION, session_id: "s", sequence_id: "2", emitted_at: "2026-01-01T00:00:00Z" } })
  expect(state.context).toBe(snapshot)
  expect(state.contextUsage).toMatchObject({ through: "2", used_tokens: "50", stable_prefix_hash: "live" })
  expect("items" in state.contextUsage!).toBe(false)
  state = apply(state, ready({ ...snapshot }))
  expect(state.contextUsage?.through).toBe("2")
  state = apply(state, ready({ ...snapshot, through: "3", used_tokens: "60" }))
  expect(state.context?.through).toBe("3")
  expect(state.contextUsage?.used_tokens).toBe("60")
})
