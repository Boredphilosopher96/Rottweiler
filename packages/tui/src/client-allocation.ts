import { CLIENT_COMMAND_LANE, type ClientCommand } from "../../../protocol/types"
import { MAX_CONTROL_RETAINED_BYTES, MAX_CLIENT_CONTROLS, MAX_CLIENT_URGENT_CONTROLS, MAX_URGENT_CONTROL_RETAINED_BYTES, MAX_URGENT_CONTROL_REPLY_RETAINED_BYTES } from "../../../protocol/types"
import { MAX_SESSION_CONTROLS_PREPARED_BYTES, MAX_SESSION_STATE_PREPARED_BYTES, MAX_SESSION_CHILDREN_PREPARED_BYTES, MAX_TODO_ITEMS, MAX_TODO_TOTAL_BYTES } from "../../../protocol/types"

export class ClientAllocationError extends Error {
  constructor(message = "client allocation admission exhausted") { super(message); this.name = "ClientAllocationError" }
}

export const MAX_CLIENT_DRAFT_BYTES = 32 * 1024 * 1024
export const CLIENT_HISTORY_BYTES = 16 * 1024 * 1024
export const CLIENT_URGENT_BYTES = MAX_CLIENT_URGENT_CONTROLS * 32 * (MAX_URGENT_CONTROL_RETAINED_BYTES + MAX_URGENT_CONTROL_REPLY_RETAINED_BYTES)
export const CLIENT_TASK_REPLY_BYTES = MAX_TODO_TOTAL_BYTES * 32 + MAX_TODO_ITEMS * 1024 + 8192
/** Each snapshot domain admits the mounted revision and its incoming decoder together. */
export const CLIENT_ALLOCATION_LIMITS = {
  history: CLIENT_HISTORY_BYTES,
  outbound: MAX_CLIENT_CONTROLS * MAX_CONTROL_RETAINED_BYTES,
  urgent: CLIENT_URGENT_BYTES,
  live: 192 * 1024 * 1024,
  decoding: 128 * 1024 * 1024,
  drafts: 2 * MAX_CLIENT_DRAFT_BYTES,
  controls: 2 * MAX_SESSION_CONTROLS_PREPARED_BYTES,
  metadata: 2 * MAX_SESSION_STATE_PREPARED_BYTES,
  children: 2 * MAX_SESSION_CHILDREN_PREPARED_BYTES,
  tasks: 2 * CLIENT_TASK_REPLY_BYTES,
} as const
export type ClientAllocationDomain = keyof typeof CLIENT_ALLOCATION_LIMITS
export interface ClientAllocationLease {
  readonly bytes: number
  resize(bytes: number): void
  admit(bytes: number): void
  moveTo(domain: ClientAllocationDomain): void
  [Symbol.dispose](): void
  release(): void
}

/** Allocation credit owns collection, decoding, retained payloads and replacement overlap. */
export class ClientAllocationOwner {
  readonly #domains = new Map<ClientAllocationDomain, number>()
  #bytes = 0
  #peak = 0
  constructor(
    readonly limits: Readonly<Record<ClientAllocationDomain, number>> = CLIENT_ALLOCATION_LIMITS,
    readonly maximumBytes = Math.min(256 * 1024 * 1024, Object.values(limits).reduce((sum, bytes) => sum + bytes, 0)),
  ) {
    if (!Number.isSafeInteger(maximumBytes) || maximumBytes <= 0
      || Object.values(limits).some(bytes => !Number.isSafeInteger(bytes) || bytes <= 0)) throw new RangeError("invalid client allocation limits")
  }
  get urgentCapacity(): number { return Math.min(this.limits.urgent, Math.floor(this.maximumBytes / 32)) }
  get normalCapacity(): number { return this.maximumBytes - this.urgentCapacity }
  get usage() { return { bytes: this.#bytes, peak: this.#peak, domains: Object.fromEntries(this.#domains) } }
  canReserve(domain: ClientAllocationDomain, bytes: number, releasingBytes = 0): boolean {
    const held = this.#domains.get(domain) ?? 0
    return Number.isSafeInteger(bytes) && bytes >= 0 && Number.isSafeInteger(releasingBytes)
      && releasingBytes >= 0 && releasingBytes <= held
      && held - releasingBytes + bytes <= (domain === "urgent" ? this.urgentCapacity : this.limits[domain])
      && this.#bytes - releasingBytes + bytes <= this.maximumBytes
      && (domain === "urgent" || this.#bytes - (this.#domains.get("urgent") ?? 0) - releasingBytes + bytes <= this.normalCapacity)
  }
  reserve(domain: ClientAllocationDomain, bytes: number): ClientAllocationLease {
    let held = 0, active = true, currentDomain = domain
    const resize = (next: number) => {
      if (!active) throw new Error("client allocation lease is released")
      const change = next - held, domainBytes = (this.#domains.get(currentDomain) ?? 0) + change
      if (!this.canReserve(currentDomain, next, held)) throw new ClientAllocationError()
      this.#domains.set(currentDomain, domainBytes)
      this.#bytes += change; this.#peak = Math.max(this.#peak, this.#bytes); held = next
    }
    const moveTo = (nextDomain: ClientAllocationDomain) => {
      if (!active) throw new Error("client allocation lease is released")
      if (currentDomain === nextDomain) return
      const nextBytes = (this.#domains.get(nextDomain) ?? 0) + held
      if (nextBytes > (nextDomain === "urgent" ? this.urgentCapacity : this.limits[nextDomain])
        || (nextDomain !== "urgent" && this.#bytes - (this.#domains.get("urgent") ?? 0)
          + (currentDomain === "urgent" ? held : 0) > this.normalCapacity)) throw new ClientAllocationError()
      this.#domains.set(currentDomain, (this.#domains.get(currentDomain) ?? 0) - held)
      this.#domains.set(nextDomain, nextBytes); currentDomain = nextDomain
    }
    resize(bytes)
    const release = () => { if (!active) return; resize(0); active = false }
    return { get bytes() { return held }, resize, admit: resize, moveTo, release, [Symbol.dispose]: release }
  }
}

export function commandReplyDomain(type: ClientCommand["type"]): "urgent" | "decoding" {
  return CLIENT_COMMAND_LANE[type] === "urgent" ? "urgent" : "decoding"
}
