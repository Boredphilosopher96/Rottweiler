import { MAX_TAIL_TEXT_BYTES, utf8Prefix } from "./display-buffer"
import {
  createStreamingTail,
  type RottweilerState,
  type StreamingTail,
  type TranscriptEntry
} from "./model"

export const MAX_COMPACTION_STREAM_BYTES = 256 * 1_024

export const MAX_RETAINED_TRANSCRIPT_ENTRIES = 256

export const MAX_RETAINED_TURN_PROJECTIONS = 256

export function retainTranscriptEntry(
  transcript: RottweilerState["transcript"],
  entry: TranscriptEntry,
): RottweilerState["transcript"] {
  return [
    ...transcript.slice(-(MAX_RETAINED_TRANSCRIPT_ENTRIES - 1)),
    entry,
  ]
}

export function retainRecentTurns(
  current: RottweilerState["turns"],
  turnId: string,
  turn: RottweilerState["turns"][string],
): RottweilerState["turns"] {
  const next = { ...current, [turnId]: turn }
  let excess = Object.keys(next).length - MAX_RETAINED_TURN_PROJECTIONS
  if (excess <= 0) return next
  for (const [id, projection] of Object.entries(next)) {
    if (excess <= 0) break
    if (id === turnId || projection.status === "running") continue
    delete next[id]
    excess -= 1
  }
  return next
}

export function currentTurnId(state: RottweilerState): string {
  if (state.streamingTail !== null) return state.streamingTail.turnId
  const turns = Object.values(state.turns)
  for (let index = turns.length - 1; index >= 0; index -= 1) {
    if (turns[index]?.status === "running") return turns[index]!.turnId
  }
  return "0"
}

export function appendTailText(tail: StreamingTail, kind: "text" | "thinking", delta: string): StreamingTail {
  const budget = tail.displayBudget[kind]
  const bytes = Buffer.byteLength(delta)
  const retained = budget.omittedBytes > 0 ? ""
    : bytes + budget.bytes <= MAX_TAIL_TEXT_BYTES ? delta : utf8Prefix(delta, MAX_TAIL_TEXT_BYTES - budget.bytes)
  const retainedBytes = Buffer.byteLength(retained)
  return {
    ...tail, [kind]: tail[kind] + retained, displayBudget: {
      ...tail.displayBudget,
      [kind]: { bytes: budget.bytes + retainedBytes, omittedBytes: Math.min(Number.MAX_SAFE_INTEGER, budget.omittedBytes + bytes - retainedBytes) },
    }
  }
}

export function updateTail(
  current: StreamingTail | null,
  turnId: string,
  update: (tail: StreamingTail) => StreamingTail,
): StreamingTail {
  const tail =
    current?.turnId === turnId
      ? current
      : createStreamingTail({
        turnId,
        text: "",
        thinking: "",
        citations: [],
        toolCallIds: [],
        finished: null,
      })
  return update(tail)
}

export function attachToolToTail(
  current: StreamingTail | null,
  turnId: string,
  toolCallId: string,
): StreamingTail {
  return updateTail(current, turnId, (tail) => ({
    ...tail,
    toolCallIds: tail.toolCallIds.includes(toolCallId)
      ? tail.toolCallIds
      : [...tail.toolCallIds, toolCallId],
  }))
}
