// Pure LRU retention core for the experimental instant back/forward cache.
// Keyed by (tabId, historyIndex). Generic over the retained value V (a live
// hidden view in production; anything in tests). All eviction returns the
// evicted entries so the caller can tear down the underlying resource.

export interface CacheKey {
  tabId: string
  index: number
}

export interface RetainedEntry<V> {
  tabId: string
  index: number
  url: string
  value: V
  lastUsed: number
}

export class PageCacheStore<V> {
  private map = new Map<string, RetainedEntry<V>>()
  private clock = 0
  private readonly cap: number

  constructor(cap: number) {
    if (cap < 0) throw new Error('PageCacheStore cap must be >= 0')
    this.cap = cap
  }

  private key(tabId: string, index: number): string {
    return `${tabId}::${index}`
  }

  has(tabId: string, index: number): boolean {
    return this.map.has(this.key(tabId, index))
  }

  /** Peek/consume a retained value, marking it most-recently-used. */
  get(tabId: string, index: number): V | undefined {
    const e = this.map.get(this.key(tabId, index))
    if (!e) return undefined
    e.lastUsed = ++this.clock
    return e.value
  }

  /**
   * Retain a value for (tabId, index). Replaces any existing entry at that key
   * (returned for teardown) and evicts LRU entries beyond the cap. Returns every
   * entry the caller must dispose of.
   */
  put(tabId: string, index: number, url: string, value: V): RetainedEntry<V>[] {
    const evicted: RetainedEntry<V>[] = []
    const k = this.key(tabId, index)
    const existing = this.map.get(k)
    if (existing) { evicted.push(existing); this.map.delete(k) }
    if (this.cap === 0) return [...evicted, { tabId, index, url, value, lastUsed: ++this.clock }]
    this.map.set(k, { tabId, index, url, value, lastUsed: ++this.clock })
    while (this.map.size > this.cap) {
      let lruKey: string | null = null
      let lru = Infinity
      for (const [mk, e] of this.map) if (e.lastUsed < lru) { lru = e.lastUsed; lruKey = mk }
      if (lruKey === null) break
      evicted.push(this.map.get(lruKey)!)
      this.map.delete(lruKey)
    }
    return evicted
  }

  /** Remove and return a single entry (e.g. when swapping it back in as live). */
  take(tabId: string, index: number): RetainedEntry<V> | undefined {
    const k = this.key(tabId, index)
    const e = this.map.get(k)
    if (e) this.map.delete(k)
    return e
  }

  /** Evict every entry belonging to a tab (tab closed / profile switch). */
  evictTab(tabId: string): RetainedEntry<V>[] {
    const out: RetainedEntry<V>[] = []
    for (const [k, e] of [...this.map]) if (e.tabId === tabId) { out.push(e); this.map.delete(k) }
    return out
  }

  clear(): RetainedEntry<V>[] {
    const out = [...this.map.values()]
    this.map.clear()
    return out
  }

  size(): number { return this.map.size }

  keys(): CacheKey[] {
    return [...this.map.values()].map((e) => ({ tabId: e.tabId, index: e.index }))
  }
}
