import type { TodoReadResult, TodoSnapshot } from "../protocol"
import { parseU64 } from "../transport/types"

/** One authoritative task snapshot; full tool results are never task-state checkpoints. */
export interface TodoState {
  readonly snapshot: TodoSnapshot
  readonly through: string | null
  readonly requiredThrough: string | null
  readonly phase: "idle" | "loading" | "ready" | "failed"
}

export function emptyTodos(): TodoState {
  return { snapshot: { items: [] }, through: null, requiredThrough: null, phase: "idle" }
}

export function invalidateTodos(state: TodoState, through: string | null): TodoState {
  return { ...emptyTodos(), requiredThrough: later(state.requiredThrough, through), phase: "loading" }
}

export function commitTodos(state: TodoState, snapshot: TodoSnapshot, through: string): TodoState {
  if (!covers(through, state.requiredThrough) || !covers(through, state.through)) return state
  return { snapshot, through, requiredThrough: through, phase: "ready" }
}

export function readTodos(state: TodoState, result: TodoReadResult): TodoState {
  if (result.type === "catching_up") {
    // A query finishing behind a newer live commit cannot erase that snapshot.
    if (state.phase === "ready" && covers(state.through, result.target)) return state
    return { ...state, phase: "loading" }
  }
  const { through, snapshot } = result.todos
  if (!covers(through, state.requiredThrough) || !covers(through, state.through)) return state
  return { snapshot, through, requiredThrough: through, phase: "ready" }
}

function covers(candidate: string | null, required: string | null): boolean {
  if (required === null) return true
  const left = parseU64(candidate), right = parseU64(required)
  return left !== null && right !== null && left >= right
}
function later(left: string | null, right: string | null): string | null {
  return covers(left, right) ? left : right
}
