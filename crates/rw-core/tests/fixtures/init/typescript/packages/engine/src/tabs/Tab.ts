import { WebContentsView, type Session, type WebContents } from 'electron'
import { EventEmitter } from 'node:events'
import { randomUUID } from 'node:crypto'
import type { TabState } from '@shared/types'
import { resolveInput } from '../navigation/resolveInput'
import { INTERNAL_SCHEMES } from '@shared/permissions'
import { errorPageHtml } from '../navigation/errorPage'
import { classifySecurity } from '../navigation/security'
import type { ContextInput } from '../menus/ContextMenuManager'

interface TabOpts {
  url?: string
  backgroundThrottling?: boolean
}

/**
 * One browser tab. Holds untrusted remote content in a sandboxed WebContentsView.
 * Can hibernate (destroy its view, keep a light record) and wake (recreate it and
 * restore url + scroll) - that swap is why `view` is created through a factory and
 * accessed via ensureView().
 */
export class Tab extends EventEmitter {
  readonly id = randomUUID()
  spaceId: string

  private readonly session: Session
  private backgroundThrottling: boolean
  private _view: WebContentsView | null

  private _title = 'New Tab'
  private _favicon: string | null = null
  private _loading = false
  private _url = ''
  private _certError: string | null = null
  private _hibernated = false
  private _scroll = { x: 0, y: 0 }
  private _pinned = false
  private _muted = false
  private _zoom = 1
  private _suppressUrlUpdate = false
  private _groupId: string | null = null

  constructor(spaceId: string, session: Session, opts: TabOpts = {}) {
    super()
    this.spaceId = spaceId
    this.session = session
    this.backgroundThrottling = opts.backgroundThrottling ?? true
    this._view = this.createView()
    if (opts.url) this.navigate(opts.url)
  }

  private createView(): WebContentsView {
    const view = new WebContentsView({
      webPreferences: {
        session: this.session,
        sandbox: true,
        contextIsolation: true,
        nodeIntegration: false,
        backgroundThrottling: this.backgroundThrottling
      }
    })
    view.setBackgroundColor('#ffffff')
    this.wire(view.webContents)
    return view
  }

  private wire(wc: WebContents): void {
    wc.on('page-title-updated', (_e, title) => {
      this._title = title
      this.changed()
    })
    wc.on('page-favicon-updated', (_e, favicons) => {
      this._favicon = favicons[0] ?? null
      this.changed()
    })
    wc.on('page-title-updated', (_e, title) => {
      if (this._url) this.emit('navigated', { url: this._url, title })
    })
    wc.on('did-start-loading', () => {
      this._loading = true
      this.changed()
    })
    wc.on('did-stop-loading', () => {
      this._loading = false
      this.changed()
    })
    wc.on('did-navigate', (_e, url) => {
      if (this._suppressUrlUpdate) {
        this._suppressUrlUpdate = false
        this.changed()
        return
      }
      this._url = url
      this._certError = null
      this.emit('navigated', { url, title: this._title })
      this.changed()
    })
    wc.on('did-fail-load', (_e, code, description, validatedURL, isMainFrame) => {
      if (isMainFrame && code <= -200 && code >= -219) this._certError = description || 'certificate error'
      if (isMainFrame && code !== -3 && validatedURL) this.loadError(code, description, validatedURL)
    })
    wc.on('context-menu', (_e, params) => {
      this.emit('context-menu', {
        x: params.x,
        y: params.y,
        linkURL: params.linkURL,
        srcURL: params.srcURL,
        selectionText: params.selectionText,
        isEditable: params.isEditable,
        mediaType: params.mediaType,
        misspelledWord: params.misspelledWord,
        dictionarySuggestions: params.dictionarySuggestions,
        editFlags: {
          canCut: params.editFlags.canCut,
          canCopy: params.editFlags.canCopy,
          canPaste: params.editFlags.canPaste,
          canSelectAll: params.editFlags.canSelectAll
        }
      } satisfies ContextInput)
    })
    wc.on('found-in-page', (_e, result) => {
      this.emit('found-in-page', {
        activeMatch: result.activeMatchOrdinal,
        totalMatches: result.matches
      })
    })
    wc.on('did-navigate-in-page', (_e, url, isMainFrame) => {
      if (isMainFrame) {
        this._url = url
        this.changed()
      }
    })
    wc.on('will-navigate', (e, url) => {
      if (this.isExternal(url)) {
        e.preventDefault()
        this.emit('external-url', url)
      }
    })
    wc.setWindowOpenHandler(({ url }) => {
      if (this.isExternal(url)) this.emit('external-url', url)
      else this.emit('open-url', url)
      return { action: 'deny' }
    })
  }

  /** true for schemes that belong to an OS app (zoommtg, slack, mailto, tel, ...),
   *  i.e. anything Sora doesn't render itself. */
  private isExternal(url: string): boolean {
    const scheme = /^([a-z][a-z0-9+.-]*):/i.exec(url)?.[1]?.toLowerCase()
    return Boolean(scheme) && !INTERNAL_SCHEMES.includes(scheme as string)
  }

  currentUrl(): string {
    return this._url
  }

  private changed(): void {
    this.emit('updated')
  }

  get hibernated(): boolean {
    return this._hibernated
  }

  /** the live view, waking the tab if it was hibernated. */
  get webContentsId(): number | null {
    return this._view && !this._view.webContents.isDestroyed() ? this._view.webContents.id : null
  }

  ensureView(): WebContentsView {
    if (this._hibernated || !this._view) this.wake()
    return this._view as WebContentsView
  }

  /** the current view without waking (for detach/destroy). */
  current(): WebContentsView | null {
    return this._view
  }

  setBackgroundThrottling(on: boolean): void {
    this.backgroundThrottling = on
    if (this._view && !this._view.webContents.isDestroyed()) {
      this._view.webContents.setBackgroundThrottling(on)
    }
  }

  navigate(input: string, searchTemplate?: string): void {
    const view = this.ensureView()
    void view.webContents.loadURL(resolveInput(input, searchTemplate))
  }

  back(): void {
    const wc = this.ensureView().webContents
    if (wc.canGoBack()) wc.goBack()
  }

  forward(): void {
    const wc = this.ensureView().webContents
    if (wc.canGoForward()) wc.goForward()
  }

  reload(): void {
    this.ensureView().webContents.reload()
  }

  stop(): void {
    this._view?.webContents.stop()
  }

  async hibernate(): Promise<void> {
    if (this._hibernated || !this._view) return
    const wc = this._view.webContents
    try {
      this._scroll = (await wc.executeJavaScript('({x:window.scrollX,y:window.scrollY})')) as {
        x: number
        y: number
      }
    } catch {
      // page may block eval; keep last known scroll
    }
    if (!wc.isDestroyed()) wc.close()
    this._view = null
    this._hibernated = true
    this.changed()
  }

  wake(): void {
    if (!this._hibernated && this._view) return
    this._view = this.createView()
    this._hibernated = false
    const wc = this._view.webContents
    const url = this._url
    const scroll = this._scroll
    wc.once('did-finish-load', () => {
      void wc.executeJavaScript(`window.scrollTo(${scroll.x}, ${scroll.y})`).catch(() => {})
    })
    if (url && url !== 'about:blank') void wc.loadURL(url)
    this.changed()
  }

  find(query: string, opts: { forward?: boolean; findNext?: boolean; matchCase?: boolean } = {}): void {
    if (!query) return
    this.ensureView().webContents.findInPage(query, opts)
  }

  findStop(): void {
    this._view?.webContents.stopFindInPage('clearSelection')
  }

  get zoom(): number {
    return this._zoom
  }

  setZoom(factor: number): void {
    this._zoom = factor
    if (this._view && !this._view.webContents.isDestroyed()) this._view.webContents.setZoomFactor(factor)
    this.changed()
  }

  get pinned(): boolean {
    return this._pinned
  }

  setPinned(on: boolean): void {
    this._pinned = on
    this.changed()
  }

  setMuted(on: boolean): void {
    this._muted = on
    if (this._view && !this._view.webContents.isDestroyed()) this._view.webContents.setAudioMuted(on)
    this.changed()
  }

  get groupId(): string | null {
    return this._groupId
  }

  setGroup(id: string | null): void {
    this._groupId = id
    this.changed()
  }

  private loadError(code: number, description: string, url: string): void {
    this._suppressUrlUpdate = true
    this._url = url
    const html = errorPageHtml(code, description, url)
    void this.ensureView().webContents.loadURL('data:text/html;charset=utf-8,' + encodeURIComponent(html))
    this.changed()
  }

  serialize(): TabState {
    const live = this._view && !this._view.webContents.isDestroyed() ? this._view.webContents : null
    return {
      id: this.id,
      spaceId: this.spaceId,
      url: this._url || (live ? live.getURL() : ''),
      title: this._title,
      favicon: this._favicon,
      loading: this._loading,
      canGoBack: live ? live.canGoBack() : false,
      canGoForward: live ? live.canGoForward() : false,
      hibernated: this._hibernated,
      zoom: this._zoom,
      pinned: this._pinned,
      muted: this._muted,
      audible: live ? live.isCurrentlyAudible() : false,
      secure: /^https:/i.test(this._url),
      security: classifySecurity({ url: this._url || (live ? live.getURL() : ''), certError: this._certError }),
      groupId: this._groupId
    }
  }

  destroy(): void {
    const wc = this._view?.webContents
    if (wc && !wc.isDestroyed()) wc.close()
    this._view = null
  }
}
