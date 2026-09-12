import type { CacheLease, CacheReservation, ClientCache } from "./cache"
import { retainedJsonBytes } from "../retained-json"
import type { ReplyAllocation } from "../transport/reply-allocation"

/** One reply owns collection, decoding and its final retained cache value. */
export class CacheRead<Value> implements ReplyAllocation {
  readonly #reservation: CacheReservation<Value>
  readonly #capacity: number
  constructor(cache: ClientCache<Value>) {
    this.#capacity = cache.capacityBytes
    const reservation = cache.reserve(1024)
    if (reservation === null) throw new Error("client cache is full with active readers")
    this.#reservation = reservation
  }
  admit(bytes: number): void {
    this.#reservation.admit(bytes)
  }
  commit(key: string, value: Value): CacheLease<Value> {
    // In-memory readers may provide already constructed values; they enter the same cache owner.
    const bytes = retainedJsonBytes(value, this.#capacity) + 96 + key.length * 2
    this.admit(bytes)
    return this.#reservation.commit(key, value)
  }
  release(): void { this.#reservation.release() }
}
