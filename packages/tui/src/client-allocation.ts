import { MAX_SESSION_CONTROLS_PREPARED_BYTES, MAX_SESSION_STATE_PREPARED_BYTES, MAX_SESSION_CHILDREN_PREPARED_BYTES, MAX_TODO_ITEMS, MAX_TODO_TOTAL_BYTES } from "../../../protocol/types"

export const CLIENT_HISTORY_BYTES = 16 * 1024 * 1024
export const CLIENT_TASK_REPLY_BYTES = MAX_TODO_TOTAL_BYTES * 32 + MAX_TODO_ITEMS * 1024 + 8192
/** Each snapshot domain admits the mounted revision and its incoming decoder together. */
export const CLIENT_ALLOCATION_LIMITS = {
  history: CLIENT_HISTORY_BYTES,
  controls: 2 * MAX_SESSION_CONTROLS_PREPARED_BYTES,
  metadata: 2 * MAX_SESSION_STATE_PREPARED_BYTES,
  children: 2 * MAX_SESSION_CHILDREN_PREPARED_BYTES,
  tasks: 2 * CLIENT_TASK_REPLY_BYTES,
} as const
export type ClientAllocationDomain = keyof typeof CLIENT_ALLOCATION_LIMITS
export interface ClientAllocationLease {
  readonly bytes: number
  resize(bytes: number): void
  release(): void
}

/** Allocation credit owns collection, decoding, retained payloads and replacement overlap. */
export class ClientAllocationOwner {
  readonly #domains = new Map<ClientAllocationDomain, number>()
  #bytes = 0
  #peak = 0
  constructor(
    readonly limits: Readonly<Record<ClientAllocationDomain, number>> = CLIENT_ALLOCATION_LIMITS,
    readonly maximumBytes = Object.values(limits).reduce((sum, bytes) => sum + bytes, 0),
  ) {
    if (!Number.isSafeInteger(maximumBytes) || maximumBytes <= 0
      || Object.values(limits).some(bytes => !Number.isSafeInteger(bytes) || bytes <= 0)) throw new RangeError("invalid client allocation limits")
  }
  get usage() { return { bytes: this.#bytes, peak: this.#peak, domains: Object.fromEntries(this.#domains) } }
  canReserve(domain: ClientAllocationDomain, bytes: number, releasingBytes = 0): boolean {
    const held = this.#domains.get(domain) ?? 0
    return Number.isSafeInteger(bytes) && bytes >= 0 && Number.isSafeInteger(releasingBytes)
      && releasingBytes >= 0 && releasingBytes <= held
      && held - releasingBytes + bytes <= this.limits[domain]
      && this.#bytes - releasingBytes + bytes <= this.maximumBytes
  }
  reserve(domain: ClientAllocationDomain, bytes: number): ClientAllocationLease {
    let held = 0, active = true
    const resize = (next: number) => {
      if (!active) throw new Error("client allocation lease is released")
      const change = next - held, domainBytes = (this.#domains.get(domain) ?? 0) + change
      if (!Number.isSafeInteger(next) || next < 0 || domainBytes > this.limits[domain]
        || this.#bytes + change > this.maximumBytes) throw new Error("client allocation admission exhausted")
      this.#domains.set(domain, domainBytes)
      this.#bytes += change; this.#peak = Math.max(this.#peak, this.#bytes); held = next
    }
    resize(bytes)
    return { get bytes() { return held }, resize,
      release: () => { if (!active) return; resize(0); active = false } }
  }
}
