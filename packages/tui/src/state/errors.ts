import type { EngineError } from "../protocol"
import { boundedUtf8 } from "./display-buffer"
import type { RottweilerState } from "./model"

export const MAX_ERROR_HISTORY = 64
export interface ErrorHistoryEntry {
  readonly id: number
  readonly category: EngineError["category"]
  readonly code: string
  readonly message: string
  readonly retryable: boolean
}

/** Retain scalar diagnostics only; arbitrary provider details never enter history. */
export function appendSessionError(state: RottweilerState, error: EngineError): RottweilerState {
  const clean = (value: string, bytes: number) => boundedUtf8(value, bytes)
    .replace(/[\u0000-\u001f\u007f-\u009f]/g, " ").replace(/\s+/g, " ").trim()
  const entry: ErrorHistoryEntry = {
    id: (state.errorHistory.at(-1)?.id ?? 0) + 1,
    category: error.category, code: clean(error.code, 128),
    message: clean(error.message, 2048) || "No additional details provided",
    retryable: error.retryable,
  }
  return { ...state, errors: [...state.errors.slice(-63), error],
    errorHistory: [...state.errorHistory.slice(-(MAX_ERROR_HISTORY - 1)), entry] }
}
