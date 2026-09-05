import type { EngineEvent } from "./protocol"
import { directSessionRead, type SessionReader, type SessionReadTarget } from "./session-reader"
import { invalidateTodos, readTodos, type TodoState } from "./state/todos"

interface TodoControllerOptions {
  readonly reader: Pick<SessionReader, "todos">
  readonly state: () => TodoState
  readonly update: (state: TodoState) => void
}

/** One exact task snapshot and one read/timer, scoped to the presented session. */
export class TodoController {
  readonly #options: TodoControllerOptions
  #session: SessionReadTarget | null = null
  #timer: ReturnType<typeof setTimeout> | null = null
  #request: AbortController | null = null
  #disposed = false
  constructor(options: TodoControllerOptions) { this.#options = options }

  open(session: SessionReadTarget, through: string | null = null): void {
    this.reset()
    if (this.#disposed) return
    this.#session = session
    this.#options.update(invalidateTodos(this.#options.state(), through))
    this.#read()
  }

  event(event: EngineEvent): void {
    if (this.#disposed) return
    if (event.type === "session_history_ready" || event.type === "session_replay_completed") {
      this.open(this.#session?.sessionId === event.session_id ? this.#session : directSessionRead(event.session_id), event.through_sequence)
    } else if (event.type === "conversation_rewound" && this.#session?.sessionId === event.meta.session_id) {
      // The reducer invalidates synchronously before a stale query can arrive.
      this.#read()
    }
  }

  retry(): void {
    if (this.#disposed || this.#options.state().phase !== "failed") return
    this.#options.update({ ...this.#options.state(), phase: "loading" })
    this.#read()
  }

  reset(): void {
    if (this.#timer !== null) clearTimeout(this.#timer)
    this.#timer = null
    this.#request?.abort()
    this.#request = null
    this.#session = null
  }

  dispose(): void { this.reset(); this.#disposed = true }

  #read(): void {
    if (this.#disposed || this.#session === null || this.#request !== null) return
    if (this.#timer !== null) clearTimeout(this.#timer)
    this.#timer = null
    const session = this.#session
    const request = new AbortController()
    this.#request = request
    void this.#options.reader.todos(session, request.signal).then(result => {
      if (this.#request !== request) return
      this.#options.update(readTodos(this.#options.state(), result))
    }).catch(() => {
      if (this.#request !== request) return
      // A successful live commit can settle task state while an older read fails.
      if (this.#options.state().phase !== "ready") {
        this.#options.update({ ...this.#options.state(), phase: "failed" })
      }
    }).finally(() => {
      if (this.#request !== request) return
      this.#request = null
      if (this.#options.state().phase !== "loading") return
      this.#timer = setTimeout(() => { this.#timer = null; this.#read() }, 50)
    })
  }
}
