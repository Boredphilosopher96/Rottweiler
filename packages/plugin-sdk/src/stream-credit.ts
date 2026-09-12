import { PROTOCOL_LIMITS } from "./generated/protocol-3"

/** One producer may wait; grants are bounded by the host's fixed window. */
export class StreamCredit {
  #events = 0
  #bytes = 0
  #waiter: { bytes: number; resolve(): void; reject(error: Error): void } | undefined
  #error: Error | undefined
  readonly #cancel = () => this.close(new Error("provider stream cancelled"))

  constructor(private readonly signal: AbortSignal) {
    signal.addEventListener("abort", this.#cancel, { once: true })
    if (signal.aborted) this.#cancel()
  }

  grant(events: number, bytes: number): void {
    if (!Number.isSafeInteger(events) || events < 0 || !Number.isSafeInteger(bytes) || bytes < 0
      || this.#events + events > PROTOCOL_LIMITS.providerWindowEvents
      || this.#bytes + bytes > PROTOCOL_LIMITS.providerWindowBytes) {
      throw new Error("provider credit exceeds the negotiated window")
    }
    if (this.#error !== undefined) return
    this.#events += events
    this.#bytes += bytes
    const waiter = this.#waiter
    if (waiter !== undefined && this.#events > 0 && this.#bytes >= waiter.bytes) {
      this.#waiter = undefined
      this.#events -= 1
      this.#bytes -= waiter.bytes
      waiter.resolve()
    }
  }

  take(bytes: number): Promise<void> {
    if (!Number.isSafeInteger(bytes) || bytes < 1 || bytes > PROTOCOL_LIMITS.providerWindowBytes) {
      return Promise.reject(new Error("provider event exceeds its byte window"))
    }
    if (this.#error !== undefined) return Promise.reject(this.#error)
    if (this.#waiter !== undefined) return Promise.reject(new Error("concurrent provider credit wait"))
    if (this.#events > 0 && this.#bytes >= bytes) {
      this.#events -= 1
      this.#bytes -= bytes
      return Promise.resolve()
    }
    return new Promise((resolve, reject) => { this.#waiter = { bytes, resolve, reject } })
  }

  close(error = new Error("provider stream closed")): void {
    this.signal.removeEventListener("abort", this.#cancel)
    this.#error ??= error
    this.#waiter?.reject(this.#error)
    this.#waiter = undefined
  }
}
