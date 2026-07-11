import type { FeatureModule, FeatureContext } from '../features/Feature'
import { PageCacheStore, type RetainedEntry } from './pageCacheStore'

/** A retained live page view. In production this is a hidden WebContentsView-like
 *  handle; the only thing the cache needs from it is a way to tear it down. */
export interface PageView {
  destroy(): void
}

/**
 * SORA EXPERIMENTAL — instant back/forward beyond Chromium's BFCache.
 *
 * For opted-in tabs, retain the last N navigated pages as fully live (hidden)
 * views keyed by (tabId, historyIndex) instead of tearing them down, so
 * back/forward across more entries is instant — at the cost of RAM. Capped LRU,
 * off by default: the user trades memory for latency knowingly.
 *
 * This class owns the retention bookkeeping (a pure PageCacheStore); the Tab
 * view-swap seam calls retain()/take()/evictTab(). Evicted/torn-down views are
 * disposed through the injected disposer so no live view leaks.
 */
export class PageVersionCache implements FeatureModule {
  readonly id = 'experimental.pageVersionCache'
  readonly name = 'Instant back/forward (page cache)'
  readonly description = 'Keep recent pages live in memory for instant navigation. Uses more RAM.'
  readonly experimental = true
  readonly settingKey = 'experimentalPageCache' as const

  static readonly DEFAULT_CAP = 6

  private store: PageCacheStore<PageView>
  private active = false
  private readonly dispose: (v: PageView) => void

  constructor(
    cap: number = PageVersionCache.DEFAULT_CAP,
    dispose: (v: PageView) => void = (v) => v.destroy()
  ) {
    this.store = new PageCacheStore<PageView>(cap)
    this.dispose = dispose
  }

  activate(_ctx: FeatureContext): void {
    this.active = true
  }

  deactivate(): void {
    this.active = false
    for (const e of this.store.clear()) this.dispose(e.value)
  }

  isActive(): boolean { return this.active }
  retainedCount(): number { return this.store.size() }

  /** Retain the outgoing view for (tabId, index); disposes any views evicted by the cap. */
  retain(tabId: string, index: number, url: string, view: PageView): void {
    if (!this.active) { this.dispose(view); return }
    for (const e of this.store.put(tabId, index, url, view)) this.dispose(e.value)
  }

  /** Reclaim a retained view to swap back in as live. Undefined if not cached. */
  take(tabId: string, index: number): PageView | undefined {
    if (!this.active) return undefined
    return this.store.take(tabId, index)?.value
  }

  has(tabId: string, index: number): boolean {
    return this.store.has(tabId, index)
  }

  /** Tab closed / profile switched — drop and dispose its retained views. */
  evictTab(tabId: string): void {
    for (const e of this.store.evictTab(tabId)) this.dispose(e.value)
  }
}

export type { RetainedEntry }
