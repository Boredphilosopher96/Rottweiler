import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { EventEmitter } from 'node:events'
import type { Session } from 'electron'
import { parseChromeManifest } from './chromeManifest'
import type { ChromeExtensionRecord, ChromeManifest } from '../contract/chromeExtensions'

const EMPTY_MANIFEST: ChromeManifest = {
  name: '', version: '', manifestVersion: 3, description: '',
  permissions: [], optionalPermissions: [], hostPermissions: [],
  contentScripts: [], background: 'none',
  action: { kind: 'none', defaultTitle: '', defaultPopup: null, defaultIcon: null },
  icons: {}, optionsPage: null
}

/** Runtime support for loading Chrome web extensions in this Electron build. */
export function chromeExtensionSupport(session: Session | null | undefined): 'none' | 'basic' | 'full' {
  if (!session) return 'none'
  // Electron 35+ exposes session.extensions (fuller chrome.* API surface)
  if ('extensions' in session && (session as unknown as { extensions?: unknown }).extensions) return 'full'
  if (typeof (session as { loadExtension?: unknown }).loadExtension === 'function') return 'basic'
  return 'none'
}

/**
 * Loads and tracks Chrome web extensions (unpacked / .crx). Bound to a session
 * getter so extensions land in the session the active tabs actually use. The
 * manifest is parsed + normalized by the pure chromeManifest module; the record
 * (incl. any load error) is surfaced to the UI management page.
 */
export class ChromeExtensionHost extends EventEmitter {
  private records = new Map<string, ChromeExtensionRecord>()

  constructor(private readonly getSession: () => Session) { super() }

  support(): 'none' | 'basic' | 'full' {
    try { return chromeExtensionSupport(this.getSession()) } catch { return 'none' }
  }

  async loadUnpacked(dir: string): Promise<ChromeExtensionRecord> {
    let record: ChromeExtensionRecord
    try {
      const raw = JSON.parse(readFileSync(join(dir, 'manifest.json'), 'utf8'))
      const parsed = parseChromeManifest(raw)
      const session = this.getSession()
      const loaded = await session.loadExtension(dir, { allowFileAccess: false })
      record = {
        id: loaded.id,
        manifest: parsed.manifest ?? EMPTY_MANIFEST,
        path: dir,
        enabled: true,
        source: 'unpacked',
        loadError: parsed.ok ? null : parsed.errors.join('; ')
      }
    } catch (e) {
      record = {
        id: 'error-' + Math.random().toString(36).slice(2, 8),
        manifest: EMPTY_MANIFEST, path: dir, enabled: false,
        source: 'unpacked', loadError: e instanceof Error ? e.message : String(e)
      }
    }
    this.records.set(record.id, record)
    this.emit('changed')
    return record
  }

  async setEnabled(id: string, on: boolean): Promise<void> {
    const rec = this.records.get(id)
    if (!rec) return
    try {
      const session = this.getSession()
      if (on && !rec.enabled) await session.loadExtension(rec.path, { allowFileAccess: false })
      else if (!on && rec.enabled) session.removeExtension(id)
      rec.enabled = on
      this.emit('changed')
    } catch { /* leave state as-is on failure */ }
  }

  remove(id: string): void {
    const rec = this.records.get(id)
    if (!rec) return
    try { this.getSession().removeExtension(id) } catch { /* already gone */ }
    this.records.delete(id)
    this.emit('changed')
  }

  list(): ChromeExtensionRecord[] {
    return [...this.records.values()]
  }
}
