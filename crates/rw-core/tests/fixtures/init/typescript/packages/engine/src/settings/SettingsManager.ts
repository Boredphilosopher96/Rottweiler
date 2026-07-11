import { EventEmitter } from 'node:events'
import { Store } from '../persistence/Store'
import { DEFAULT_SETTINGS, type SoraSettings } from '@shared/settings'

/** Typed, persisted settings for one profile. Emits 'changed'. */
export class SettingsManager extends EventEmitter {
  private store: Store<SoraSettings>

  constructor(dir: string) {
    super()
    this.store = new Store<SoraSettings>(dir, 'settings', DEFAULT_SETTINGS)
  }

  get(): SoraSettings {
    return this.store.get()
  }

  patch(patch: Partial<SoraSettings>): void {
    this.store.set(patch)
    this.emit('changed')
  }
}
