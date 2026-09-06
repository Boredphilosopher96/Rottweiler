import type { SessionCompactionState } from "../../../protocol/types"
import { expect, test } from "bun:test"
import { PROTOCOL_VERSION, type EngineEvent } from "../src/protocol"
import { createInitialState, engineEvent, reduceRottweilerState } from "../src/state"
import { restoreCompaction } from "../src/state/compaction-recovery"
const meta = (sequence_id: string) => ({ protocol_version: PROTOCOL_VERSION, session_id: "s", sequence_id, emitted_at: "2026-01-01T00:00:00Z" })
const apply = (state: ReturnType<typeof createInitialState>, event: EngineEvent) => reduceRottweilerState(state, engineEvent(event), "s")
const snapshot = (revision: string, text: string): SessionCompactionState => ({ started: "1", summary_turn_id: "c", attempt: 0, revision,
  text: { text, truncated: false }, thinking: { text: "", truncated: false } })
const delta = (revision: string, text: string): EngineEvent => ({ type: "compaction_text_delta", session_id: "s", summary_turn_id: "c", started: "1", revision, attempt: 0, text })

test("a missing transient revision requires a snapshot and never joins discontiguous text", () => {
  let state = apply(createInitialState(), { type: "compaction_started", meta: meta("1"), reason: "automatic" })
  state = apply(state, { type: "compaction_attempt_started", session_id: "s", summary_turn_id: "c", started: "1", revision: "1", attempt: 0 })
  state = apply(state, delta("2", "prefix"))
  state = apply(state, delta("4", "suffix"))
  expect(state.compaction.text).toBe("prefix")
  expect(state.recovery.compaction).toMatchObject({ revision: "2", observed: "4", stale: true })
  expect(restoreCompaction(state, snapshot("3", "prefix-middle"), "1").compaction).toBe(state.compaction)
  state = { ...state, ...restoreCompaction(state, snapshot("4", "prefix-middle-suffix"), "1") }
  expect(state.recovery.compaction?.stale).toBe(false)
  state = apply(state, delta("4", "duplicated"))
  state = apply(state, delta("5", "-next"))
  expect(state.compaction.text).toBe("prefix-middle-suffix-next")
})

test("source and attempt fences preserve unresolved work until an exact clearing snapshot", () => {
  let state = createInitialState()
  state = { ...state, ...restoreCompaction(state, snapshot("8", "latest"), "1") }
  expect(restoreCompaction(state, null, null).compaction).toBe(state.compaction)
  expect(apply(state, { ...delta("99", "foreign"), started: "0" } as EngineEvent)).toBe(state)
  const cleared = restoreCompaction(state, null, "1")
  expect(cleared.compaction.active).toBe(false)
  expect(cleared.recovery.compaction).toBeNull()
})
