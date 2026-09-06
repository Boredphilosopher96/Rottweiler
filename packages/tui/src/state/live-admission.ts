import {
  MAX_PLUGIN_STATUS_BYTES, MAX_SESSION_PLUGIN_STATUSES, MAX_CITATION_TEXT_BYTES, MAX_TURN_CITATIONS, MAX_TURN_CITATION_TEXT_BYTES,
  MAX_PENDING_QUESTION_REQUESTS, MAX_QUESTION_BYTES,
} from "../../../../protocol/types"
import type { EngineEvent } from "../protocol"
import { jsonEncodedBytes } from "../json-size"
import { EngineProtocolError } from "../transport/errors"
import type { RottweilerState } from "./model"

export function citationBytes(uri: string, title: string | null): number {
  return Buffer.byteLength(uri) + (title === null ? 0 : Buffer.byteLength(title))
}

/** Reject an inadmissible live payload before its cursor or projection can advance. */
export function assertLiveAdmission(state: RottweilerState, event: EngineEvent): void {
  if (event.type === "plugin_status_changed") {
    if (state.recovery.metadataThrough !== null && BigInt(event.meta.sequence_id) <= BigInt(state.recovery.metadataThrough)) return
    if (Buffer.byteLength(event.status) > MAX_PLUGIN_STATUS_BYTES
      || (event.status !== "" && !Object.hasOwn(state.pluginStatuses, event.plugin_id)
        && Object.keys(state.pluginStatuses).length >= MAX_SESSION_PLUGIN_STATUSES)) {
      throw new EngineProtocolError("plugin statuses exceed the source-owned admission limit")
    }
  } else if (event.type === "question_asked") {
    const pending = Object.keys(state.questions).length
    if ((state.questions[event.question_id] === undefined && pending >= MAX_PENDING_QUESTION_REQUESTS)
      || event.question.id !== event.question_id
      || jsonEncodedBytes(event.question, MAX_QUESTION_BYTES) > MAX_QUESTION_BYTES) {
      throw new EngineProtocolError("unresolved questions exceed the source-owned admission limit")
    }
  } else if (event.type === "citation_delta") {
    const tail = state.streamingTail?.turnId === event.turn_id ? state.streamingTail : null
    const bytes = citationBytes(event.uri, event.title ?? null)
    if (bytes > MAX_CITATION_TEXT_BYTES || (tail?.citations.length ?? 0) >= MAX_TURN_CITATIONS
      || (tail?.citationBytes ?? 0) + bytes > MAX_TURN_CITATION_TEXT_BYTES) {
      throw new EngineProtocolError("turn citations exceed the source-owned admission limit")
    }
  }
}
