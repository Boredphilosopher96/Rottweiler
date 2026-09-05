import { TRANSCRIPT_TAIL_TEXT_BYTES } from "../../../../protocol/types"
import type { SessionCompactionState } from "../../../../protocol/types"
import type { EngineEvent } from "../protocol"
import type { RottweilerState } from "./model"
import { utf8Prefix } from "./display-buffer"

export interface CompactionFence {
  readonly started: string
  readonly revision: string
  readonly observed: string
  readonly stale: boolean
  readonly textTruncated: boolean
  readonly thinkingTruncated: boolean
}
type Progress = Extract<EngineEvent, { type: "compaction_attempt_started" | "compaction_text_delta" | "compaction_thinking_delta" }>

export function restoreCompaction(state: RottweilerState, snapshot: SessionCompactionState | null, through: string | null): Pick<RottweilerState, "compaction" | "recovery"> {
  const previous = state.recovery.compaction
  if (snapshot === null) {
    if (previous !== null && (through === null || BigInt(through) < BigInt(previous.started))) return { compaction: state.compaction, recovery: state.recovery }
    return { compaction: { active: false, reason: null, summaryTurnId: null, attempt: null, reclaimedTokens: null, text: "", thinking: "" },
      recovery: { ...state.recovery, compaction: null } }
  }
  if (previous !== null && (BigInt(previous.started) > BigInt(snapshot.started)
    || (previous.started === snapshot.started && BigInt(previous.observed) > BigInt(snapshot.revision)))) {
    return { compaction: state.compaction, recovery: state.recovery }
  }
  return { compaction: { active: true, reason: null, summaryTurnId: snapshot.summary_turn_id, attempt: snapshot.attempt,
    reclaimedTokens: null, text: snapshot.text.text, thinking: snapshot.thinking.text },
    recovery: { ...state.recovery, compaction: { started: snapshot.started, revision: snapshot.revision, observed: snapshot.revision,
      stale: false, textTruncated: snapshot.text.truncated, thinkingTruncated: snapshot.thinking.truncated } } }
}

/** Revision gaps request a fresh bounded snapshot; missing transient bytes are never concatenated away. */
export function compactionProgress(state: RottweilerState, event: Progress): RottweilerState {
  const previous = state.recovery.compaction
  if (previous !== null && (BigInt(event.started) < BigInt(previous.started)
    || (event.started === previous.started && BigInt(event.revision) <= BigInt(previous.revision)))) return state
  const same = previous?.started === event.started
  const fence: CompactionFence = same ? previous! : { started: event.started, revision: "0", observed: "0", stale: true,
    textTruncated: false, thinkingTruncated: false }
  const observed = BigInt(event.revision) > BigInt(fence.observed) ? event.revision : fence.observed
  if (!same || fence.stale || BigInt(event.revision) !== BigInt(fence.revision) + 1n) {
    return { ...state, recovery: { ...state.recovery, compaction: { ...fence, observed, stale: true } } }
  }
  const nextFence = { ...fence, revision: event.revision, observed }
  if (event.type === "compaction_attempt_started") {
    if (state.compaction.attempt !== null && event.attempt < state.compaction.attempt) return state
    return { ...state, compaction: { ...state.compaction, active: true, summaryTurnId: event.summary_turn_id,
      attempt: event.attempt, text: "", thinking: "" }, recovery: { ...state.recovery,
      compaction: { ...nextFence, textTruncated: false, thinkingTruncated: false } } }
  }
  if (!state.compaction.active || state.compaction.summaryTurnId !== event.summary_turn_id || state.compaction.attempt !== event.attempt) {
    return { ...state, recovery: { ...state.recovery, compaction: { ...fence, observed, stale: true } } }
  }
  const kind = event.type === "compaction_text_delta" ? "text" : "thinking"
  const flag = kind === "text" ? "textTruncated" : "thinkingTruncated"
  const remaining = Math.max(0, TRANSCRIPT_TAIL_TEXT_BYTES - Buffer.byteLength(state.compaction[kind]))
  const text = fence[flag] ? "" : utf8Prefix(event.text, remaining)
  return { ...state, compaction: { ...state.compaction, [kind]: state.compaction[kind] + text },
    recovery: { ...state.recovery, compaction: { ...nextFence, [flag]: fence[flag] || text.length !== event.text.length } } }
}

export function observeCompaction(state: RottweilerState, event: EngineEvent, sequence: string): RottweilerState {
  if (event.type === "compaction_started") return { ...state, recovery: { ...state.recovery,
    compaction: { started: sequence, revision: "0", observed: "0", stale: false, textTruncated: false, thinkingTruncated: false } } }
  if (event.type === "conversation_rewound" || ((event.type === "compaction_finished" || event.type === "compaction_failed") && !state.compaction.active)) {
    return { ...state, recovery: { ...state.recovery, compaction: null } }
  }
  return state
}
