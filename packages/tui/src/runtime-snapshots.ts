import type { ReplyAllocation } from "./transport/reply-allocation"
import { EngineProtocolError } from "./transport/errors"

interface Request { readonly sessionId: string; readonly signal: AbortSignal }

/** One decoding owner plus one coalesced request; cancellation settles before ownership transfers. */
export class SessionSnapshotReader<Event> {
  #wanted: Request | null = null
  #running: Promise<void> | null = null
  #nextReadAt = 0
  constructor(
    readonly maximumPreparedBytes: number,
    readonly read: (sessionId: string, signal: AbortSignal, allocation: ReplyAllocation) => Promise<Event>,
    readonly apply: (event: Event) => boolean,
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
        const delay = this.#nextReadAt - performance.now()
        if (delay > 0) await retryDelay(request.signal, delay)
        if (request.signal.aborted) continue
        this.#nextReadAt = performance.now() + 250
        const maximumPreparedBytes = this.maximumPreparedBytes
        const event = await this.read(request.sessionId, request.signal, {
          admit(bytes) {
            if (!Number.isSafeInteger(bytes) || bytes < 0 || bytes > maximumPreparedBytes) {
              throw new EngineProtocolError("session snapshot reply exceeds its prepared allocation limit")
            }
          },
        })
        if (!request.signal.aborted && !this.apply(event) && this.#wanted === null) {
          // A newer source transition won the reply race. Demand stays coalesced.
          if (!request.signal.aborted && this.#wanted === null) this.#wanted = request
        }
      } catch (error) {
        if (!request.signal.aborted) this.failed(error, request.sessionId)
      }
    } } finally { this.#running = null }
  }
}

function retryDelay(signal: AbortSignal, delay: number): Promise<void> {
  return new Promise(resolve => {
    const finish = () => { clearTimeout(timer); signal.removeEventListener("abort", finish); resolve() }
    const timer = setTimeout(finish, delay)
    signal.addEventListener("abort", finish, { once: true })
    if (signal.aborted) finish()
  })
}
