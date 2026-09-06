import { CLIENT_HISTORY_BYTES, ClientAllocationOwner, type ClientAllocationLease } from "../client-allocation"
import { retainedJsonBytes } from "../retained-json"

/** A retained value remains charged until its last mounted reader releases it. */
export interface CacheLease<Value> {
  readonly value: Value
  release(): void
}

/** Admission before source collection/decoding; cancellation keeps credit until settlement. */
export interface CacheReservation<Value> {
  /** Grow the admitted allocation before collecting or decoding more input. */
  admit(bytes: number): void
  commit(key: string, value: Value): CacheLease<Value>
  release(): void
}

export interface CacheLimits {
  readonly bytes: number
  readonly entries: number
}

interface Entry<Value> {
  readonly allocation: ClientAllocationLease
  readonly value: Value
  readonly bytes: number
  readers: number
  resident: boolean
}

export interface CacheUsage {
  readonly bytes: number
  readonly entries: number
  readonly residentEntries: number
  readonly pinnedEntries: number
}

/** One allocation owner for all history, child, artifact and document payloads. */
export class ClientCache<Value> {
  readonly #limits: CacheLimits
  readonly #resident = new Map<string, Entry<Value>>()
  #bytes = 0
  #entries = 0
  #pinned = 0

  constructor(limits: CacheLimits = { bytes: CLIENT_HISTORY_BYTES, entries: 2048 }, readonly allocations = new ClientAllocationOwner()) {
    if (!Number.isSafeInteger(limits.bytes) || limits.bytes <= 0
      || !Number.isSafeInteger(limits.entries) || limits.entries <= 0) {
      throw new RangeError("invalid client cache limits")
    }
    this.#limits = limits
  }

  get capacityBytes(): number { return this.#limits.bytes }

  get usage(): CacheUsage {
    return {
      bytes: this.#bytes,
      entries: this.#entries,
      residentEntries: this.#resident.size,
      pinnedEntries: this.#pinned,
    }
  }

  /** Charge once on admission; readers and frames never remeasure a value. */
  insert(key: string, value: Value): boolean {
    const bytes = retainedJsonBytes(value, this.#limits.bytes) + 96 + key.length * 2
    if (bytes > this.#limits.bytes) return false
    const previous = this.#resident.get(key)
    const reclaimPrevious = previous !== undefined && previous.readers === 0
    let availableBytes = this.#limits.bytes - this.#bytes + (reclaimPrevious ? previous.bytes : 0)
    let availableEntries = this.#limits.entries - this.#entries + (reclaimPrevious ? 1 : 0)
    const evictions: string[] = []
    for (const [candidateKey, entry] of this.#resident) {
      if (availableBytes >= bytes && availableEntries >= 1) break
      if (candidateKey === key || entry.readers !== 0) continue
      evictions.push(candidateKey)
      availableBytes += entry.bytes
      availableEntries += 1
    }
    if (availableBytes < bytes || availableEntries < 1) return false
    const releasing = evictions.reduce((sum, candidate) => sum + this.#resident.get(candidate)!.bytes, reclaimPrevious ? previous!.bytes : 0)
    if (!this.allocations.canReserve("history", bytes, releasing)) return false
    for (const candidate of evictions) this.remove(candidate)
    this.remove(key)
    let allocation: ClientAllocationLease
    try { allocation = this.allocations.reserve("history", bytes) } catch { return false }
    this.#resident.set(key, { value, bytes, allocation, readers: 0, resident: true })
    this.#bytes += bytes
    this.#entries += 1
    return true
  }

  reserve(maximumBytes: number): CacheReservation<Value> | null {
    if (!Number.isSafeInteger(maximumBytes) || maximumBytes <= 0 || maximumBytes > this.#limits.bytes) return null
    let bytes = this.#limits.bytes - this.#bytes
    let entries = this.#limits.entries - this.#entries
    const evictions: string[] = []
    for (const [key, entry] of this.#resident) {
      if (bytes >= maximumBytes && entries >= 1) break
      if (entry.readers !== 0) continue
      evictions.push(key)
      bytes += entry.bytes
      entries += 1
    }
    if (bytes < maximumBytes || entries < 1) return null
    const releasing = evictions.reduce((sum, key) => sum + this.#resident.get(key)!.bytes, 0)
    if (!this.allocations.canReserve("history", maximumBytes, releasing)) return null
    for (const key of evictions) this.remove(key)
    let allocation: ClientAllocationLease
    try { allocation = this.allocations.reserve("history", maximumBytes) } catch { return null }
    this.#bytes += maximumBytes
    this.#entries += 1
    let active = true
    const admit = (required: number) => {
      if (!active) throw new Error("cache reservation is released")
      if (!Number.isSafeInteger(required) || required < 0 || required > this.#limits.bytes) throw new Error("read exceeds the client cache allowance")
      if (required <= maximumBytes) return
      let available = this.#limits.bytes - this.#bytes
      const evictions: string[] = []
      for (const [key, entry] of this.#resident) {
        if (available >= required - maximumBytes) break
        if (entry.readers !== 0) continue
        evictions.push(key)
        available += entry.bytes
      }
      if (available < required - maximumBytes) throw new Error("client cache is full with active readers")
      const releasing = evictions.reduce((sum, key) => sum + this.#resident.get(key)!.bytes, 0)
      if (!this.allocations.canReserve("history", required - maximumBytes, releasing)) throw new Error("client allocation admission exhausted")
      for (const key of evictions) this.remove(key)
      allocation.resize(required)
      this.#bytes += required - maximumBytes
      maximumBytes = required
    }
    return {
      admit,
      release: () => {
        if (!active) return
        active = false
        allocation.release()
        this.#bytes -= maximumBytes
        this.#entries -= 1
      },
      commit: (key, value) => {
        if (!active) throw new Error("cache reservation is released")
        const actual = retainedJsonBytes(value, maximumBytes) + 96 + key.length * 2
        if (actual > maximumBytes) throw new Error("decoded value exceeds reserved cache allocation")
        this.remove(key)
        allocation.resize(actual)
        this.#bytes -= maximumBytes - actual
        this.#resident.set(key, { value, bytes: actual, allocation, readers: 0, resident: true })
        active = false
        const lease = this.lease(key)
        if (lease === null) throw new Error("committed cache value is missing")
        return lease
      },
    }
  }

  /** A lease pins its exact revision, including after replacement or invalidation. */
  lease(key: string): CacheLease<Value> | null {
    const entry = this.#resident.get(key)
    if (entry === undefined) return null
    this.#resident.delete(key)
    this.#resident.set(key, entry)
    if (entry.readers++ === 0) this.#pinned += 1
    return new ReaderLease(entry, released => {
      released.readers -= 1
      if (released.readers === 0) {
        this.#pinned -= 1
        if (!released.resident) this.#release(released)
      }
    })
  }

  remove(key: string): void {
    const entry = this.#resident.get(key)
    if (entry === undefined) return
    this.#resident.delete(key)
    entry.resident = false
    if (entry.readers === 0) this.#release(entry)
  }

  clear(): void {
    for (const key of this.#resident.keys()) this.remove(key)
  }

  #release(entry: Entry<Value>): void {
    entry.allocation.release()
    this.#bytes -= entry.bytes
    this.#entries -= 1
  }
}

/** Clearing the lease severs its payload reference even if its caller retains it. */
class ReaderLease<Value> implements CacheLease<Value> {
  #entry: Entry<Value> | null
  readonly #release: (entry: Entry<Value>) => void

  constructor(entry: Entry<Value>, release: (entry: Entry<Value>) => void) {
    this.#entry = entry
    this.#release = release
  }

  get value(): Value {
    if (this.#entry === null) throw new Error("cache lease is released")
    return this.#entry.value
  }

  release(): void {
    const entry = this.#entry
    if (entry === null) return
    this.#entry = null
    this.#release(entry)
  }
}
