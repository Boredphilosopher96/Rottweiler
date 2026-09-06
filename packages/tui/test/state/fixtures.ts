import type { EngineEvent } from "../../src/protocol"
import {
  PROTOCOL_VERSION,
  type SubagentResult
} from "../../src/protocol"
import {
  engineEvent,
  reduceRottweilerState,
  type RottweilerState
} from "../../src/state"


export function meta(sequence: string) {
  return {
    protocol_version: PROTOCOL_VERSION,
    session_id: "session-state",
    sequence_id: sequence,
    emitted_at: "2026-01-01T00:00:00Z",
  }
}

export function metaAt(sequence: string, emittedAt: string) {
  return { ...meta(sequence), emitted_at: emittedAt }
}

export function reduce(state: RottweilerState, event: EngineEvent): RottweilerState {
  return reduceRottweilerState(state, engineEvent(event))
}

export function childResult(
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
