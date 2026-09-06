import { CLIENT_TASK_REPLY_BYTES } from "./client-allocation"
import type { EngineEvent, TodoReadResult } from "./protocol"
import { SessionSnapshotReader } from "./runtime-snapshots"
import { directSessionRead, type SessionReader, type SessionReadTarget } from "./session-reader"
import { invalidateTodos, readTodos, type TodoState } from "./state/todos"

interface TodoControllerOptions {
  readonly reader: Pick<SessionReader, "todos">
  readonly state: () => TodoState
  readonly update: (state: TodoState) => void
}
interface TaskRead { readonly target: SessionReadTarget; readonly generation: object }

/** One exact task snapshot and one settled read owner across session changes. */
export class TodoController {
  readonly #options: TodoControllerOptions
  readonly #reader: SessionSnapshotReader<TodoReadResult, TaskRead>
  #session: SessionReadTarget | null = null
  #generation: object = {}
  #controller: AbortController | null = null
  #disposed = false
  constructor(options: TodoControllerOptions) {
    this.#options = options
    this.#reader = new SessionSnapshotReader(
      CLIENT_TASK_REPLY_BYTES,
      (request, signal, allocation) => options.reader.todos(request.target, signal, allocation),
      result => {
        options.update(readTodos(options.state(), result))
        return options.state().phase !== "loading"
      },
      (_error, request) => {
        if (request.generation !== this.#generation || options.state().phase === "ready") return
        options.update({ ...options.state(), phase: "failed" })
      },
    )
  }

  open(session: SessionReadTarget, through: string | null = null): void {
    this.reset()
    if (this.#disposed) return
    this.#session = session
    this.#controller = new AbortController()
    this.#options.update(invalidateTodos(this.#options.state(), through))
    this.#read()
  }

  event(event: EngineEvent): void {
    if (this.#disposed) return
    if (event.type === "session_history_ready" || event.type === "session_replay_completed") {
      this.open(this.#session?.sessionId === event.session_id ? this.#session : directSessionRead(event.session_id), event.through_sequence)
    } else if (event.type === "conversation_rewound" && this.#session?.sessionId === event.meta.session_id) {
      this.#read()
    }
  }

  retry(): void {
    if (this.#disposed || this.#options.state().phase !== "failed") return
    this.#options.update({ ...this.#options.state(), phase: "loading" })
    this.#read()
  }

  reset(): void {
    this.#generation = {}
    this.#controller?.abort()
    this.#controller = null
    this.#session = null
  }
  settle(): Promise<void> { return this.#reader.settle() }
  dispose(): void { this.reset(); this.#disposed = true }

  #read(): void {
    if (this.#disposed || this.#session === null || this.#controller === null) return
    void this.#reader.refresh({ target: this.#session, generation: this.#generation }, this.#controller.signal)
  }
}
