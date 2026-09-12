import { PROTOCOL_LIMITS, type ToolProgress } from "./generated/protocol-3"

/** Keeps one latest observation and at most one physical write per invocation. */
export class ToolProgressReporter {
  #latest: ToolProgress | undefined
  #worker: Promise<void> | undefined
  #closed = false
  #sequence = 0
  #lastWrite = -Infinity
  #failure: Error | undefined
  #stop = new AbortController()

  constructor(
    private readonly write: (sequence: number, progress: ToolProgress) => Promise<void>,
    private readonly delivered: () => void,
    private readonly failed: () => void,
  ) {}

  report(progress: ToolProgress): void {
    if (this.#closed) throw new Error("tool progress is closed")
    this.#latest = snapshot(progress)
    if (this.#worker === undefined) this.#start()
  }

  async finish(): Promise<void> {
    this.#closed = true
    this.#latest = undefined
    this.#stop.abort()
    await this.#worker
    if (this.#failure !== undefined) throw this.#failure
  }

  #start(): void {
    this.#worker = this.#run().catch(() => {
      this.#failure = new Error("tool progress delivery failed")
      this.#closed = true
      this.#latest = undefined
      this.failed()
    }).finally(() => {
      this.#worker = undefined
      if (!this.#closed && this.#latest !== undefined) this.#start()
    })
  }

  async #run(): Promise<void> {
    while (!this.#closed && this.#latest !== undefined) {
      const delay = this.#lastWrite + PROTOCOL_LIMITS.progressIntervalMs - performance.now()
      if (delay > 0) await this.#pause(delay)
      if (this.#closed) return
      const progress = this.#latest
      this.#latest = undefined
      if (progress === undefined) return
      this.#sequence += 1
      await this.write(this.#sequence, progress)
      this.#lastWrite = performance.now()
      if (!this.#closed) this.delivered()
    }
  }

  #pause(delay: number): Promise<void> {
    return new Promise(resolve => {
      const finish = () => {
        clearTimeout(timer)
        this.#stop.signal.removeEventListener("abort", finish)
        resolve()
      }
      const timer = setTimeout(finish, delay)
      this.#stop.signal.addEventListener("abort", finish, { once: true })
      if (this.#stop.signal.aborted) finish()
    })
  }
}

function snapshot(progress: ToolProgress): ToolProgress {
  if (progress === null || typeof progress !== "object"
    || Object.keys(progress).some(key => key !== "message" && key !== "amount")
    || typeof progress.message !== "string" || progress.message.length === 0
    || progress.message.length > PROTOCOL_LIMITS.maxProgressMessageChars * 2
    || [...progress.message].length > PROTOCOL_LIMITS.maxProgressMessageChars
    || new TextEncoder().encode(progress.message).byteLength > PROTOCOL_LIMITS.maxProgressMessageBytes
    || /[\p{Cc}]/u.test(progress.message)) throw new Error("invalid tool progress")
  const amount = progress.amount
  if (amount === undefined || amount === null) return { message: progress.message }
  if (typeof amount !== "object"
    || Object.keys(amount).some(key => key !== "completed" && key !== "total")
    || !Number.isInteger(amount.completed) || !Number.isInteger(amount.total)
    || amount.completed < 0 || amount.total < 1 || amount.total > 0xffff_ffff
    || amount.completed > amount.total) throw new Error("invalid tool progress amount")
  return { message: progress.message, amount: { completed: amount.completed, total: amount.total } }
}
