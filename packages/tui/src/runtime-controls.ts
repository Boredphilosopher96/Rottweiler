import { MAX_SESSION_CONTROLS_PREPARED_BYTES } from "../../../protocol/types"
import type { EngineEvent } from "./protocol"
import type { ReplyAllocation } from "./transport/reply-allocation"
import { EngineProtocolError } from "./transport/errors"

type ControlsEvent = Extract<EngineEvent, { type: "session_controls_ready" }>
interface Request { readonly sessionId: string; readonly signal: AbortSignal }

/** One decoding owner plus one coalesced request; cancellation settles before ownership transfers. */
export class SessionControlReader {
  #wanted: Request | null = null
  #running: Promise<void> | null = null
  constructor(
    readonly read: (sessionId: string, signal: AbortSignal, allocation: ReplyAllocation) => Promise<ControlsEvent>,
    readonly apply: (event: ControlsEvent) => boolean,
    readonly failed: (error: unknown, sessionId: string) => void,
  ) {}

  refresh(sessionId: string, signal: AbortSignal): Promise<void> {
    this.#wanted = { sessionId, signal }
    if (this.#running !== null) return this.#running
    const running = Promise.resolve().then(() => this.#drain())
    this.#running = running
    return running
  }

  settle(): Promise<void> { return this.#running ?? Promise.resolve() }

  async #drain(): Promise<void> {
    try { while (this.#wanted !== null) {
      const request = this.#wanted
      this.#wanted = null
      if (request.signal.aborted) continue
      try {
        const event = await this.read(request.sessionId, request.signal, {
          admit(bytes) {
            if (!Number.isSafeInteger(bytes) || bytes < 0 || bytes > MAX_SESSION_CONTROLS_PREPARED_BYTES) {
              throw new EngineProtocolError("session control reply exceeds its prepared allocation limit")
            }
          },
        })
        if (!request.signal.aborted && !this.apply(event) && this.#wanted === null) {
          // A later durable control transition won the reply race. Retry at most four reads/second.
          await retryDelay(request.signal)
          if (!request.signal.aborted && this.#wanted === null) this.#wanted = request
        }
      } catch (error) {
        if (!request.signal.aborted) this.failed(error, request.sessionId)
      }
    } } finally { this.#running = null }
  }
}

function retryDelay(signal: AbortSignal): Promise<void> {
  return new Promise(resolve => {
    const finish = () => { clearTimeout(timer); signal.removeEventListener("abort", finish); resolve() }
    const timer = setTimeout(finish, 250)
    signal.addEventListener("abort", finish, { once: true })
    if (signal.aborted) finish()
  })
}
