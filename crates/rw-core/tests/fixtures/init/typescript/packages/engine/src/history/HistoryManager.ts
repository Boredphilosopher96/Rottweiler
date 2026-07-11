import { EventEmitter } from 'node:events'
import { Store } from '../persistence/Store'
import type { HistoryEntry } from '@shared/history'

interface HistData {
  entries: HistoryEntry[]
}

/**
 * Core visit history for one profile: one entry per URL with visit count and
 * first/last timestamps (frecency-ready). Persisted JSON now; moves to SQLite
 * when search gets slow. Emits 'changed'.
 */
export class HistoryManager extends EventEmitter {
  private store: Store<HistData>

  constructor(dir: string) {
    super()
    this.store = new Store<HistData>(dir, 'history', { entries: [] })
  }

  get entries(): HistoryEntry[] {
    return this.store.get().entries
  }

  /** newest-first, capped, for the UI (omnibox suggestions filter this client-side). */
  recent(limit = 100): HistoryEntry[] {
    return [...this.entries].sort((a, b) => b.lastVisit - a.lastVisit).slice(0, limit)
  }

  record(url: string, title: string): void {
    if (!/^https?:\/\//i.test(url) && !url.startsWith('file:')) return
    const now = Date.now()
    const entries = this.entries
    const existing = entries.find((e) => e.url === url)
    if (existing) {
      existing.lastVisit = now
      existing.visitCount += 1
      if (title) existing.title = title
      this.store.replace({ entries })
    } else {
      this.store.replace({
        entries: [...entries, { url, title: title || url, firstVisit: now, lastVisit: now, visitCount: 1 }].slice(-5000)
      })
    }
    this.emit('changed')
  }

  setTitle(url: string, title: string): void {
    if (!title) return
    const entries = this.entries
    const e = entries.find((x) => x.url === url)
    if (e && e.title !== title) {
      e.title = title
      this.store.replace({ entries })
      this.emit('changed')
    }
  }

  search(query: string, limit = 50): HistoryEntry[] {
    const q = query.toLowerCase()
    return this.entries
      .filter((e) => e.url.toLowerCase().includes(q) || e.title.toLowerCase().includes(q))
      .sort((a, b) => b.visitCount - a.visitCount || b.lastVisit - a.lastVisit)
      .slice(0, limit)
  }

  clear(): void {
    this.store.replace({ entries: [] })
    this.emit('changed')
  }
}
