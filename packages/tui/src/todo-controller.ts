import type { EngineEvent } from "./protocol"
import type { ProjectionRequestBroker } from "./projection-requests"
import { invalidateTodos, type TodoState } from "./state/todos"

interface TodoControllerOptions {
  readonly requests: ProjectionRequestBroker
  readonly state: () => TodoState
  readonly update: (state: TodoState) => void
}

/** A single pending task read and retry timer, retired with its session/renderer. */
export class TodoController {
  readonly #options: TodoControllerOptions
  #timer: ReturnType<typeof setTimeout> | null = null
  #disposed = false
  constructor(options: TodoControllerOptions) { this.#options = options }

  event(event: EngineEvent): void {
    if (this.#disposed) return
    if (event.type === "session_history_ready" || event.type === "session_replay_completed") {
      this.#options.update(invalidateTodos(this.#options.state(), event.through_sequence ?? null))
      this.#request()
    } else if (event.type === "conversation_rewound") {
      // The reducer invalidates synchronously before any stale query can arrive.
      this.#request()
    } else if (event.type === "todos_read" && this.#options.state().phase !== "ready") {
      this.#schedule()
    }
  }

  failed(): void {
    this.reset()
    this.#options.update({ ...invalidateTodos(this.#options.state(), null), phase: "failed" })
  }

  retry(): void {
    if (this.#disposed || this.#options.state().phase !== "failed") return
    this.#options.update({ ...this.#options.state(), phase: "loading" })
    this.#request()
  }

  reset(): void {
    if (this.#timer !== null) clearTimeout(this.#timer)
    this.#timer = null
  }

  dispose(): void { this.reset(); this.#disposed = true }

  #request(): void {
    this.reset()
    if (this.#disposed) return
    if (this.#options.requests.current("todos") !== null) return
    this.#options.requests.command({ type: "get_todos" })
  }

  #schedule(): void {
    if (this.#timer !== null) return
    this.#timer = setTimeout(() => { this.#timer = null; this.#request() }, 50)
  }
}
