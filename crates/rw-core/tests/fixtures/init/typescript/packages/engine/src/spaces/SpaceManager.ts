import { EventEmitter } from 'node:events'
import { randomUUID } from 'node:crypto'
import { Store } from '../persistence/Store'
import type { SpaceState } from '@shared/types'

interface SpacesData {
  spaces: SpaceState[]
  activeId: string
}

/** Persisted Spaces for one profile. Seeds two defaults on first run. */
export class SpaceManager extends EventEmitter {
  private store: Store<SpacesData>

  constructor(dir: string) {
    super()
    this.store = new Store<SpacesData>(dir, 'spaces', { spaces: [], activeId: '' })
    if (this.store.get().spaces.length === 0) {
      const personal: SpaceState = { id: randomUUID(), name: 'Personal', color: 'oklch(0.62 0.13 250)', private: false }
      const work: SpaceState = { id: randomUUID(), name: 'Work', color: 'oklch(0.60 0.13 150)', private: false }
      this.store.replace({ spaces: [personal, work], activeId: personal.id })
    }
  }

  get all(): SpaceState[] {
    return this.store.get().spaces
  }

  get activeId(): string {
    return this.store.get().activeId
  }

  get(id: string): SpaceState | undefined {
    return this.all.find((s) => s.id === id)
  }

  isPrivate(id: string): boolean {
    return Boolean(this.get(id)?.private)
  }

  create(name: string, color: string, isPrivate = false): SpaceState {
    const s: SpaceState = { id: randomUUID(), name, color, private: isPrivate }
    const d = this.store.get()
    this.store.replace({ spaces: [...d.spaces, s], activeId: d.activeId })
    this.emit('changed')
    return s
  }

  activate(id: string): void {
    const d = this.store.get()
    if (d.spaces.some((s) => s.id === id)) {
      this.store.replace({ spaces: d.spaces, activeId: id })
      this.emit('changed')
    }
  }
}
