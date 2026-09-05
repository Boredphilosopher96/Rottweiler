import { retainedJsonBytes } from "../retained-json"

/** A retained value remains charged until its last mounted reader releases it. */
export interface CacheLease<Value> {
  readonly value: Value
  release(): void
}

export interface CacheLimits {
  readonly bytes: number
  readonly entries: number
}

interface Entry<Value> {
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

  constructor(limits: CacheLimits = { bytes: 16 * 1024 * 1024, entries: 2048 }) {
    if (!Number.isSafeInteger(limits.bytes) || limits.bytes <= 0
      || !Number.isSafeInteger(limits.entries) || limits.entries <= 0) {
      throw new RangeError("invalid client cache limits")
    }
    this.#limits = limits
  }

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
    for (const candidate of evictions) this.remove(candidate)
    this.remove(key)
    this.#resident.set(key, { value, bytes, readers: 0, resident: true })
    this.#bytes += bytes
    this.#entries += 1
    return true
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
