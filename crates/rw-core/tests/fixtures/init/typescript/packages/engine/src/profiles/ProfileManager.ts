import { app } from 'electron'
import { join } from 'node:path'
import { randomUUID } from 'node:crypto'
import { EventEmitter } from 'node:events'
import { Store } from '../persistence/Store'
import { ProfileContext } from './ProfileContext'
import type { ProfileMeta } from '@shared/types'

interface ProfileIndex {
  profiles: ProfileMeta[]
  activeId: string
}

/**
 * Owns the set of profiles and which is active. Each profile roots its data
 * under userData/profiles/<id>/. One active profile per window for now.
 */
export class ProfileManager extends EventEmitter {
  private index: Store<ProfileIndex>
  private contexts = new Map<string, ProfileContext>()
  private baseDir: string

  constructor() {
    super()
    this.baseDir = join(app.getPath('userData'), 'profiles')
    this.index = new Store<ProfileIndex>(this.baseDir, 'index', { profiles: [], activeId: '' })

    if (this.index.get().profiles.length === 0) {
      const first: ProfileMeta = { id: randomUUID(), name: 'Default', color: 'oklch(0.62 0.13 250)' }
      this.index.replace({ profiles: [first], activeId: first.id })
    }
    // materialize the active context eagerly
    this.contextFor(this.index.get().activeId)
  }

  get all(): ProfileMeta[] {
    return this.index.get().profiles
  }

  get activeId(): string {
    return this.index.get().activeId
  }

  private dirFor(id: string): string {
    return join(this.baseDir, id)
  }

  contextFor(id: string): ProfileContext {
    let ctx = this.contexts.get(id)
    if (!ctx) {
      const meta = this.all.find((p) => p.id === id)
      if (!meta) throw new Error(`unknown profile ${id}`)
      ctx = new ProfileContext(meta, this.dirFor(id))
      this.contexts.set(id, ctx)
    }
    return ctx
  }

  active(): ProfileContext {
    return this.contextFor(this.activeId)
  }

  create(name: string, color: string): ProfileMeta {
    const meta: ProfileMeta = { id: randomUUID(), name, color }
    const d = this.index.get()
    this.index.replace({ profiles: [...d.profiles, meta], activeId: d.activeId })
    this.emit('changed')
    return meta
  }

  activate(id: string): void {
    const d = this.index.get()
    if (d.profiles.some((p) => p.id === id)) {
      this.index.replace({ profiles: d.profiles, activeId: id })
      this.contextFor(id)
      this.emit('activated', id)
      this.emit('changed')
    }
  }
}
