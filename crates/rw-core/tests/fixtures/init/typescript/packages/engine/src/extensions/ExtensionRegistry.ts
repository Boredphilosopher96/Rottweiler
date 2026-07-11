import { EventEmitter } from 'node:events'
import { Store } from '../persistence/Store'
import type { Contributions, ExtensionRecord, SoraExtensionManifest } from '@shared/extensions'

interface ExtData {
  records: ExtensionRecord[]
}

/**
 * Foundation for Sora's own native (GUI-modifying) extensions. Loads/enables
 * manifests and merges their contributions so the UI shell can already render
 * extension-contributed items. No sandboxed execution yet - contribution ABI only.
 * (Chrome web extensions are a separate track via electron-chrome-extensions, later.)
 */
export class ExtensionRegistry extends EventEmitter {
  private store: Store<ExtData>

  constructor(dir: string) {
    super()
    this.store = new Store<ExtData>(dir, 'extensions', { records: [] })
  }

  get records(): ExtensionRecord[] {
    return this.store.get().records
  }

  install(manifest: SoraExtensionManifest, enabled = false): void {
    const records = this.records.filter((r) => r.manifest.id !== manifest.id)
    records.push({ manifest, enabled })
    this.store.replace({ records })
    this.emit('changed')
  }

  setEnabled(id: string, enabled: boolean): void {
    const records = this.records.map((r) =>
      r.manifest.id === id ? { ...r, enabled } : r
    )
    this.store.replace({ records })
    this.emit('changed')
  }

  /** Merge contributions from all enabled extensions into one bundle for the UI. */
  contributions(): Contributions {
    const merged: Required<Contributions> = { sidebarItems: [], commands: [], themes: [] }
    for (const r of this.records) {
      if (!r.enabled || !r.manifest.contributes) continue
      const c = r.manifest.contributes
      if (c.sidebarItems) merged.sidebarItems.push(...c.sidebarItems)
      if (c.commands) merged.commands.push(...c.commands)
      if (c.themes) merged.themes.push(...c.themes)
    }
    return merged
  }
}
