import { EventEmitter } from 'node:events'
import { Store } from '../persistence/Store'
import type { ZoomLevel } from '@shared/zoom'

interface ZoomData {
  levels: Record<string, number>
}

/** Per-profile, persisted per-origin zoom factors. Emits 'changed'. */
export class ZoomStore extends EventEmitter {
  private store: Store<ZoomData>

  constructor(dir: string) {
    super()
    this.store = new Store<ZoomData>(dir, 'zoom', { levels: {} })
  }

  get(origin: string): number {
    return this.store.get().levels[origin] ?? 1
  }

  set(origin: string, factor: number): void {
    if (!origin) return
    const levels = { ...this.store.get().levels }
    if (Math.abs(factor - 1) < 1e-6) delete levels[origin]
    else levels[origin] = factor
    this.store.replace({ levels })
    this.emit('changed')
  }

  all(): ZoomLevel[] {
    return Object.entries(this.store.get().levels).map(([origin, factor]) => ({ origin, factor }))
  }
}
