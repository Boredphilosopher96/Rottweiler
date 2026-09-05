import type { TodoItem } from "../../src/protocol"
import { emptyTodos, type TodoState } from "../../src/state/todos"
export function todoState(items: TodoItem[], through: string | null = null): TodoState {
  return { ...emptyTodos(), snapshot: { items }, through, phase: "ready" }
}
