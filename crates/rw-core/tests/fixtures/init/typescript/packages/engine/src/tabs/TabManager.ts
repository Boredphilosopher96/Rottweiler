import { EventEmitter } from 'node:events'
import type { Session } from 'electron'
import { Tab } from './Tab'

/**
 * Owns the set of tabs for a window and which one is active.
 * Emits 'changed' on any structural or per-tab update, and 'open-url'
 * when a tab wants to spawn a new tab (link with target=_blank etc).
 */
export class TabManager extends EventEmitter {
  private tabs: Tab[] = []
  private _activeId: string | null = null

  get all(): Tab[] {
    return this.tabs
  }

  get activeId(): string | null {
    return this._activeId
  }

  getActive(): Tab | undefined {
    return this.tabs.find((t) => t.id === this._activeId)
  }

  get(id: string): Tab | undefined {
    return this.tabs.find((t) => t.id === id)
  }

  tabsInSpace(spaceId: string): Tab[] {
    return this.tabs.filter((t) => t.spaceId === spaceId)
  }

  create(
    spaceId: string,
    session: Session,
    opts: { url?: string; backgroundThrottling?: boolean; activate?: boolean } = {}
  ): Tab {
    const { activate = true, ...tabOpts } = opts
    const tab = new Tab(spaceId, session, tabOpts)
    tab.on('updated', () => this.emit('changed'))
    tab.on('open-url', (u: string) => this.emit('open-url', { url: u, spaceId }))
    tab.on('external-url', (u: string) => this.emit('external-url', { url: u, origin: originOf(tab.currentUrl()) }))
    tab.on('navigated', (e: { url: string; title: string }) => this.emit('navigated', { ...e, tabId: tab.id, spaceId: tab.spaceId }))
    tab.on('context-menu', (input: unknown) => this.emit('context-menu', { tabId: tab.id, input }))
    tab.on('found-in-page', (r: { activeMatch: number; totalMatches: number }) =>
      this.emit('found-in-page', { tabId: tab.id, ...r })
    )
    this.tabs.push(tab)
    if (activate) this._activeId = tab.id
    this.emit('changed')
    return tab
  }

  close(id: string): void {
    const idx = this.tabs.findIndex((t) => t.id === id)
    if (idx === -1) return
    const [tab] = this.tabs.splice(idx, 1)
    tab.destroy()
    if (this._activeId === id) {
      const next = this.tabs[idx] ?? this.tabs[idx - 1] ?? null
      this._activeId = next ? next.id : null
    }
    this.emit('changed')
  }

  activate(id: string): void {
    if (this.tabs.some((t) => t.id === id)) {
      this._activeId = id
      this.emit('changed')
    }
  }

  reorder(id: string, toIndex: number): void {
    const from = this.tabs.findIndex((t) => t.id === id)
    if (from === -1) return
    const [t] = this.tabs.splice(from, 1)
    const clamped = Math.max(0, Math.min(toIndex, this.tabs.length))
    this.tabs.splice(clamped, 0, t)
    this.emit('changed')
  }
}

function originOf(url: string): string {
  try {
    return new URL(url).origin
  } catch {
    return ''
  }
}
