import { EventEmitter } from 'node:events'
import { Store } from '../persistence/Store'
import type { DownloadRecord } from '@shared/downloads'

interface HistData {
  items: DownloadRecord[]
}

/** Per-profile persisted downloads history (survives restart). Emits 'changed'. */
export class DownloadHistoryStore extends EventEmitter {
  private store: Store<HistData>

  constructor(dir: string) {
    super()
    this.store = new Store<HistData>(dir, 'downloads', { items: [] })
  }

  get items(): DownloadRecord[] {
    return this.store.get().items
  }

  upsert(rec: DownloadRecord): void {
    const items = [rec, ...this.items.filter((r) => r.id !== rec.id)].slice(0, 500)
    this.store.replace({ items })
    this.emit('changed')
  }

  remove(id: string): void {
    this.store.replace({ items: this.items.filter((r) => r.id !== id) })
    this.emit('changed')
  }

  clear(): void {
    this.store.replace({ items: [] })
    this.emit('changed')
  }
}
