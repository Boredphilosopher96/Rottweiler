import { BaseWindow, WebContentsView, app, shell, clipboard, type Session } from 'electron'
import { detectOS } from '../platform/detectOS'
import { join } from 'node:path'
import { randomUUID } from 'node:crypto'
import { writeFile } from 'node:fs/promises'
import { TabManager } from '../tabs/TabManager'
import { LayoutManager } from '../tabs/LayoutManager'
import { HibernationManager } from '../tabs/HibernationManager'
import { SessionManager } from '../sessions/SessionManager'
import { StorageManager } from '../storage/StorageManager'
import { ProfileManager } from '../profiles/ProfileManager'
import type { ProfileContext } from '../profiles/ProfileContext'
import { SessionRestore, type RestoreData } from '../session-restore/SessionRestore'
import { SyncEngine } from '../sync/SyncEngine'
import { LocalFolderSync } from '../sync/LocalFolderSync'
import { SYNC_SCHEMA } from '../sync/merge'
import { PromptQueue } from '../permissions/PromptQueue'
import { DownloadManager } from '../downloads/DownloadManager'
import { savePageFilename, electronSaveType } from '../downloads/savePage'
import { extractArticle } from '../reader/readability'
import { READER_GATHER_SCRIPT } from '../reader/gatherScript'
import { buildAuthPromptBase } from '../permissions/authPrompt'
import { ChromeExtensionHost } from '../extensions/ChromeExtensionHost'
import { PasswordVault } from '../passwords/PasswordVault'
import { dispatchKdf } from '../crypto/kdf'
import { SafeZoning } from '../safeZoning/SafeZoning'
import { SIGNALS_SCRIPT } from '../safeZoning/guardScript'
import { RUN_SESSION_ID } from '../runtime/runSession'
import type { BrowserConfig } from '@shared/config'
import { FeatureRegistry, type NavigationEvent } from '../features/Feature'
import { PageVersionCache } from '../experimental/PageVersionCache'
import { HistoryTree } from '../features/historyTree/HistoryTree'
import { GhostTab } from '../features/ghostTab/GhostTab'
import { ReadingPositionSync } from '../features/readingPosition/ReadingPositionSync'
import { Evt } from '@shared/ipc'
import { CONTRACT_VERSION, type Capabilities, type Hello } from '@shared/version'
import type { BrowserState, Insets, LayoutState } from '@shared/types'
import type { SavePageFormat, SavePageResult } from '@shared/savePage'
import type { ReaderArticle } from '@shared/reader'
import type { AuthCredentials } from '@shared/permissions'
import type { ChromeExtensionRecord } from '@shared/chromeExtensions'
import type { VaultState, Credential, KdfAlgo } from '@shared/vault'
import type { SafeZoningMode, ZoningDecision, SafeZoningState } from '@shared/safeZoning'
import type { SyncableSnapshot } from '@shared/sync'
import type { PermissionDecision } from '@shared/permissions'
import { ContextMenuManager, type ContextInput } from '../menus/ContextMenuManager'
import { CTX, type ContextMenuModel } from '@shared/menus'
import { zoomStep } from '@shared/zoom'
import type { FindState } from '@shared/find'
import type { ClosedTab, TabGroupState } from '@shared/types'
import { suggest as computeSuggestions } from '../omnibox/suggest'
import type { Suggestion } from '@shared/omnibox'

const HOST_OS = detectOS()

const CAPS: Capabilities = {
  tabs: true,
  spaces: true,
  profiles: true,
  split: true,
  hibernation: true,
  bookmarks: true,
  settings: true,
  sync: true,
  nativeExtensions: true,
  chromeExtensions: false,
  sessionRestore: true,
  permissions: true,
  externalProtocols: true,
  downloads: true,
  history: true,
  contextMenus: true,
  findInPage: true,
  zoom: true,
  omnibox: true,
  tabGroups: true,
  privateSpaces: true,
  spellcheck: true,
  printToPdf: true,
  savePage: true,
  readerMode: true,
  passwordManager: true
}

function originOf(url: string | undefined): string {
  if (!url) return ''
  try {
    return new URL(url).origin
  } catch {
    return ''
  }
}

/**
 * A Sora window. Owns the engine. The only class aware of both UI and engine;
 * it pushes BrowserState snapshots to the UI on any change. Also the runtime home
 * for permission handling and external-protocol ("open in app") routing, which
 * depend on both the owning profile's stored policy and this window's prompt queue.
 */
export class Browser {
  readonly win: BaseWindow
  readonly chrome: WebContentsView
  readonly profiles = new ProfileManager()
  readonly tabs = new TabManager()
  readonly sessions = new SessionManager()
  readonly storage = new StorageManager()
  readonly prompts = new PromptQueue()
  readonly downloads: DownloadManager
  private chromeExt!: ChromeExtensionHost
  private caps!: Capabilities
  readonly passwords: PasswordVault
  readonly safeZoning = new SafeZoning()
  readonly features: FeatureRegistry
  readonly layout: LayoutManager
  readonly hibernation: HibernationManager
  readonly sync: SyncEngine

  private layoutState: LayoutState = { mode: 'single', paneTabIds: [] }
  private pushScheduled = false
  private dataChangedAt = Date.now()
  private profileUnbind: (() => void) | null = null
  private attachedSessions = new WeakSet<Session>()
  private navListeners: Array<(e: NavigationEvent) => void> = []
  private readonly contextMenus = new ContextMenuManager()
  private pendingContextMenu: ContextMenuModel | null = null
  private findState: FindState | null = null
  private findOpts: { matchCase?: boolean } = {}
  private closedTabs: ClosedTab[] = []
  private groups: TabGroupState[] = []

  constructor(private readonly config: BrowserConfig) {
    const w = config.window ?? {}
    this.win = new BaseWindow({
      width: w.width ?? 1280,
      height: w.height ?? 820,
      minWidth: w.minWidth ?? 640,
      minHeight: w.minHeight ?? 480,
      frame: false,
      backgroundColor: w.backgroundColor ?? '#0b0b0c'
    })
    this.layout = new LayoutManager(this.win)

    this.chrome = new WebContentsView({
      webPreferences: {
        preload: config.chrome.preloadPath,
        contextIsolation: true,
        sandbox: false
      }
    })
    this.win.contentView.addChildView(this.chrome)
    this.sizeChrome()
    this.loadChrome()

    this.hibernation = new HibernationManager({
      getTabs: () => this.tabs.all,
      getProtectedIds: () => this.protectedTabIds(),
      getPolicy: () => {
        const s = this.profile.settings.get()
        return { policy: s.tabPolicy, minutes: s.hibernateAfterMinutes }
      },
      onChange: () => this.schedulePush()
    })

    const localSyncFolder = join(app.getPath('userData'), 'sora-sync-local')
    this.sync = new SyncEngine(
      new LocalFolderSync(localSyncFolder),
      () => this.localSnapshot(),
      (snap) => this.applyRemoteSnapshot(snap)
    )
    this.sync.on('status', () => this.schedulePush())
    this.prompts.on('changed', () => this.schedulePush())
    app.on('login', (event, wc, details, authInfo, callback) => {
      if (!this.tabs.all.some((t) => t.webContentsId === wc.id)) return
      event.preventDefault()
      this.prompts.addAuth(buildAuthPromptBase(authInfo, details.url), (creds) => {
        if (creds) callback(creds.username, creds.password)
        else callback()
      })
    })
    this.downloads = new DownloadManager({
      getSettings: () => this.profile.settings.get(),
      getHistory: () => this.profile.downloads,
      onChange: () => this.schedulePush()
    })
    this.chromeExt = new ChromeExtensionHost(() => {
      const spaceId = this.profile.spaces.activeId
      return this.sessions.forSpace(this.profiles.activeId, spaceId, false).session
    })
    this.chromeExt.on('changed', () => this.schedulePush())
    this.caps = { ...CAPS, chromeExtensions: this.chromeExt.support() !== 'none' }
    this.passwords = new PasswordVault(this.profile.dir, dispatchKdf)
    this.passwords.on('changed', () => this.schedulePush())
    this.safeZoning.on('changed', () => this.schedulePush())
    this.features = new FeatureRegistry(
      [new PageVersionCache(), new HistoryTree(), new GhostTab(), new ReadingPositionSync()],
      {
        profileDir: () => this.profile.dir,
        onNavigation: (cb) => this.onNavigation(cb),
        emitChange: () => this.schedulePush()
      }
    )
    this.features.sync(this.profile.settings.get())

    this.win.on('resize', () => {
      this.sizeChrome()
      this.layout.position()
    })
    this.win.on('maximize', () => this.schedulePush())
    this.win.on('unmaximize', () => this.schedulePush())

    this.tabs.on('changed', () => {
      this.relayout()
      this.schedulePush()
    })
    this.tabs.on('open-url', ({ url, spaceId }: { url: string; spaceId: string }) =>
      this.newTab(url, spaceId, true)
    )
    this.tabs.on('external-url', ({ url, origin }: { url: string; origin: string }) =>
      this.handleExternal(url, origin)
    )
    this.tabs.on('navigated', (e: NavigationEvent) => this.onNavigated(e))
    this.tabs.on('context-menu', ({ tabId, input }: { tabId: string; input: ContextInput }) =>
      this.onContextMenu(tabId, input)
    )
    this.tabs.on(
      'found-in-page',
      ({ tabId, activeMatch, totalMatches }: { tabId: string; activeMatch: number; totalMatches: number }) =>
        this.onFound(tabId, activeMatch, totalMatches)
    )
    this.profiles.on('changed', () => this.schedulePush())

    this.bindActiveProfile()
    this.hibernation.start()

    if (this.profile.settings.get().sessionRestore) this.restoreSession()
    else this.newTab()
  }

  get profile(): ProfileContext {
    return this.profiles.active()
  }

  private bindActiveProfile(): void {
    if (this.profileUnbind) this.profileUnbind()
    const ctx = this.profile
    const onSettings = (): void => {
      this.dataChangedAt = Date.now()
      this.hibernation.applyThrottling()
      this.features.sync(ctx.settings.get())
      this.schedulePush()
    }
    const onData = (): void => {
      this.dataChangedAt = Date.now()
      this.schedulePush()
    }
    ctx.settings.on('changed', onSettings)
    ctx.bookmarks.on('changed', onData)
    ctx.spaces.on('changed', onData)
    ctx.extensions.on('changed', onData)
    ctx.permissions.on('changed', onData)
    ctx.externalProtocols.on('changed', onData)
    ctx.downloads.on('changed', onData)
    ctx.history.on('changed', onData)
    ctx.zoom.on('changed', onData)
    this.profileUnbind = (): void => {
      ctx.settings.off('changed', onSettings)
      ctx.bookmarks.off('changed', onData)
      ctx.spaces.off('changed', onData)
      ctx.extensions.off('changed', onData)
      ctx.permissions.off('changed', onData)
      ctx.externalProtocols.off('changed', onData)
      ctx.downloads.off('changed', onData)
      ctx.history.off('changed', onData)
      ctx.zoom.off('changed', onData)
    }
  }

  private loadChrome(): void {
    const c = this.config.chrome
    if (c.devUrl) void this.chrome.webContents.loadURL(c.devUrl)
    else if (c.fileEntry) void this.chrome.webContents.loadFile(c.fileEntry)
  }

  private sizeChrome(): void {
    const b = this.win.getContentBounds()
    this.chrome.setBounds({ x: 0, y: 0, width: b.width, height: b.height })
  }

  private protectedTabIds(): string[] {
    const ids = new Set<string>()
    if (this.tabs.activeId) ids.add(this.tabs.activeId)
    if (this.layoutState.mode === 'split') for (const id of this.layoutState.paneTabIds) ids.add(id)
    return [...ids]
  }

  private relayout(): void {
    let views: WebContentsView[] = []
    if (this.layoutState.mode === 'split' && this.layoutState.paneTabIds.length === 2) {
      views = this.layoutState.paneTabIds
        .map((id) => this.tabs.get(id)?.ensureView())
        .filter((v): v is WebContentsView => Boolean(v))
    } else {
      const active = this.tabs.getActive()
      if (active) views = [active.ensureView()]
    }
    if (this.tabs.activeId) this.hibernation.touch(this.tabs.activeId)
    this.layout.apply(views)
  }

  // ── sessions get their permission handler attached on first use ──
  private acquireSession(profileId: string, spaceId: string): Session {
    const ephemeral = this.profiles.contextFor(profileId).spaces.isPrivate(spaceId)
    const { session, created } = this.sessions.forSpace(profileId, spaceId, ephemeral)
    if (created || !this.attachedSessions.has(session)) {
      this.attachPermissionHandlers(session, profileId)
      this.attachedSessions.add(session)
    }
    this.downloads.attach(session)
    return session
  }

  private attachPermissionHandlers(session: Session, profileId: string): void {
    const perms = this.profiles.contextFor(profileId).permissions
    session.setPermissionRequestHandler((wc, permission, callback, details) => {
      const origin = originOf(details?.requestingUrl ?? wc?.getURL())
      const decision = perms.decide(origin, permission)
      if (decision === 'allow') return callback(true)
      if (decision === 'deny') return callback(false)
      this.prompts.add({ kind: 'permission', origin, permission }, (allow, remember) => {
        if (remember) perms.setPolicy(origin, permission, allow ? 'allow' : 'deny')
        callback(allow)
      })
    })
    session.setPermissionCheckHandler((_wc, permission, requestingOrigin) => {
      return perms.decide(requestingOrigin ?? '', permission) === 'allow'
    })
  }

  // ── external protocols ("open in app") ──
  private handleExternal(url: string, origin: string): void {
    const scheme = (/^([a-z][a-z0-9+.-]*):/i.exec(url)?.[1] ?? '').toLowerCase()
    if (!scheme) return
    const ext = this.profile.externalProtocols
    const decision = ext.decide(scheme)
    if (decision === 'allow') return void shell.openExternal(url)
    if (decision === 'deny') return
    this.prompts.add({ kind: 'external', origin, scheme, url }, (allow, remember) => {
      if (remember) ext.setPolicy(scheme, allow ? 'allow' : 'deny')
      if (allow) void shell.openExternal(url)
    })
  }

  private onNavigated(e: NavigationEvent): void {
    if (this.profile.settings.get().historyEnabled && !this.profile.spaces.isPrivate(e.spaceId))
      this.profile.history.record(e.url, e.title)
    const tab = this.tabs.get(e.tabId)
    if (tab) tab.setZoom(this.profile.zoom.get(originOf(e.url)))
    if (this.findState?.tabId === e.tabId) { tab?.findStop(); this.findState = null }
    for (const cb of this.navListeners) cb(e)
  }

  /** subscribe to navigation facts (used by feature modules). returns an unsubscribe. */
  onNavigation(cb: (e: NavigationEvent) => void): () => void {
    this.navListeners.push(cb)
    return () => {
      this.navListeners = this.navListeners.filter((f) => f !== cb)
    }
  }

  clearHistory(): void {
    this.profile.history.clear()
  }

  // ── context menus ──
  private onContextMenu(tabId: string, input: ContextInput): void {
    const tab = this.tabs.get(tabId)
    if (!tab) return
    const s = tab.serialize()
    this.pendingContextMenu = this.contextMenus.build(tabId, input, {
      canGoBack: s.canGoBack,
      canGoForward: s.canGoForward
    })
    this.schedulePush()
  }

  contextMenuAction(tabId: string, actionId: string): void {
    const tab = this.tabs.get(tabId)
    const model = this.pendingContextMenu
    this.pendingContextMenu = null
    if (!tab) return this.schedulePush()
    const wc = tab.ensureView().webContents
    const link = model?.linkURL ?? ''
    const src = model?.srcURL ?? ''
    const sel = model?.selectionText ?? ''
    switch (actionId) {
      case CTX.Back: tab.back(); break
      case CTX.Forward: tab.forward(); break
      case CTX.Reload: tab.reload(); break
      case CTX.OpenLinkNewTab: if (link) this.newTab(link, tab.spaceId); break
      case CTX.CopyLink: if (link) clipboard.writeText(link); break
      case CTX.SaveLinkAs: if (link) this.downloads.triggerViaWebContents(wc, link); break
      case CTX.OpenImageNewTab: if (src) this.newTab(src, tab.spaceId); break
      case CTX.CopyImage: if (model) wc.copyImageAt(model.x, model.y); break
      case CTX.SaveImageAs: if (src) this.downloads.triggerViaWebContents(wc, src); break
      case CTX.Copy: wc.copy(); break
      case CTX.Cut: wc.cut(); break
      case CTX.Paste: wc.paste(); break
      case CTX.SelectAll: wc.selectAll(); break
      case CTX.SearchSelection: if (sel) this.newTab(this.searchUrl(sel), tab.spaceId); break
      case CTX.ViewSource: this.newTab('view-source:' + tab.currentUrl(), tab.spaceId); break
      case CTX.Inspect: if (model) wc.inspectElement(model.x, model.y); break
      case CTX.AddToDictionary: if (model?.misspelledWord) wc.session.addWordToSpellCheckerDictionary(model.misspelledWord); break
    }
    this.schedulePush()
  }

  contextMenuClose(): void {
    this.pendingContextMenu = null
    this.schedulePush()
  }

  private searchUrl(q: string): string {
    return this.profile.settings.get().searchEngine.replace('%s', encodeURIComponent(q))
  }

  // ── find in page ──
  private onFound(tabId: string, activeMatch: number, totalMatches: number): void {
    if (this.findState && this.findState.tabId === tabId) {
      this.findState = { ...this.findState, activeMatch, totalMatches }
      this.schedulePush()
    }
  }

  find(id: string, query: string, opts: { forward?: boolean; matchCase?: boolean } = {}): void {
    const tab = this.tabs.get(id)
    if (!tab) return
    if (!query) return this.findStop(id)
    this.findState = { tabId: id, query, activeMatch: 0, totalMatches: 0 }
    this.findOpts = { matchCase: opts.matchCase }
    tab.find(query, opts)
    this.schedulePush()
  }

  findNext(id: string, forward: boolean): void {
    const tab = this.tabs.get(id)
    if (tab && this.findState?.query) tab.find(this.findState.query, { forward, findNext: true, matchCase: this.findOpts.matchCase })
  }

  findStop(id: string): void {
    this.tabs.get(id)?.findStop()
    if (this.findState?.tabId === id) this.findState = null
    this.schedulePush()
  }

  // ── zoom (per origin) ──
  zoomSet(id: string, factor: number): void {
    const tab = this.tabs.get(id)
    if (!tab) return
    tab.setZoom(factor)
    this.profile.zoom.set(originOf(tab.currentUrl()), factor)
  }
  zoomIn(id: string): void {
    const tab = this.tabs.get(id)
    if (tab) this.zoomSet(id, zoomStep(tab.zoom, 1))
  }
  zoomOut(id: string): void {
    const tab = this.tabs.get(id)
    if (tab) this.zoomSet(id, zoomStep(tab.zoom, -1))
  }
  zoomReset(id: string): void {
    this.zoomSet(id, 1)
  }

  // ── tab ops ──
  setPinned(id: string, on: boolean): void {
    this.tabs.get(id)?.setPinned(on)
  }
  setMuted(id: string, on: boolean): void {
    this.tabs.get(id)?.setMuted(on)
  }
  duplicateTab(id: string): void {
    const tab = this.tabs.get(id)
    if (tab) this.newTab(tab.currentUrl(), tab.spaceId, true)
  }
  toggleDevTools(id: string): void {
    const wc = this.tabs.get(id)?.ensureView().webContents
    if (!wc) return
    if (wc.isDevToolsOpened()) wc.closeDevTools()
    else wc.openDevTools({ mode: 'detach' })
  }

  // ── recently closed ──
  closeTab(id: string): void {
    const tab = this.tabs.get(id)
    if (tab) {
      const s = tab.serialize()
      if (s.url && s.url !== 'about:blank') {
        this.closedTabs.unshift({ url: s.url, title: s.title, spaceId: tab.spaceId, closedAt: Date.now() })
        this.closedTabs = this.closedTabs.slice(0, 25)
      }
    }
    this.tabs.close(id)
  }
  reopenClosed(): void {
    const c = this.closedTabs.shift()
    if (c) this.newTab(c.url, c.spaceId, true)
    this.schedulePush()
  }

  // ── tab groups ──
  createGroup(name: string, color: string): void {
    this.groups.push({ id: randomUUID(), name, color, collapsed: false })
    this.schedulePush()
  }
  renameGroup(id: string, name: string): void {
    const g = this.groups.find((x) => x.id === id)
    if (g) { g.name = name; this.schedulePush() }
  }
  setGroupColor(id: string, color: string): void {
    const g = this.groups.find((x) => x.id === id)
    if (g) { g.color = color; this.schedulePush() }
  }
  setGroupCollapsed(id: string, collapsed: boolean): void {
    const g = this.groups.find((x) => x.id === id)
    if (g) { g.collapsed = collapsed; this.schedulePush() }
  }
  removeGroup(id: string): void {
    this.groups = this.groups.filter((g) => g.id !== id)
    for (const t of this.tabs.all) if (t.groupId === id) t.setGroup(null)
    this.schedulePush()
  }
  setTabGroup(tabId: string, groupId: string | null): void {
    const t = this.tabs.get(tabId)
    if (!t) return
    if (groupId && !this.groups.some((g) => g.id === groupId)) return
    t.setGroup(groupId)
    this.schedulePush()
  }

  // ── spellcheck ──
  contextMenuReplace(tabId: string, word: string): void {
    this.tabs.get(tabId)?.ensureView().webContents.replaceMisspelling(word)
    this.pendingContextMenu = null
    this.schedulePush()
  }

  // ── print to pdf ──
  async printToPdf(id: string): Promise<void> {
    const tab = this.tabs.get(id)
    if (!tab) return
    try {
      const data = await tab.ensureView().webContents.printToPDF({})
      const dir = this.profile.settings.get().downloadDir || app.getPath('downloads')
      await writeFile(join(dir, `sora-${Date.now()}.pdf`), data)
    } catch {
      // page not printable (e.g. still loading); ignore
    }
    this.schedulePush()
  }

  // ── save page as (HTML / MHTML) ──
  async savePageAs(id: string, format: SavePageFormat): Promise<SavePageResult> {
    const tab = this.tabs.get(id)
    if (!tab) return { ok: false, format, error: 'no such tab' }
    try {
      const st = tab.serialize()
      const dir = this.profile.settings.get().downloadDir || app.getPath('downloads')
      const full = join(dir, savePageFilename(st.title, st.url, format))
      await tab.ensureView().webContents.savePage(full, electronSaveType(format))
      this.schedulePush()
      return { ok: true, path: full, format }
    } catch (e) {
      return { ok: false, format, error: e instanceof Error ? e.message : String(e) }
    }
  }

  // ── reader mode: gather blocks in-page, score them here ──
  async readerExtract(id: string): Promise<ReaderArticle | null> {
    const tab = this.tabs.get(id)
    if (!tab) return null
    try {
      const raw = await tab.ensureView().webContents.executeJavaScript(READER_GATHER_SCRIPT, true) as {
        title: string; byline: string | null; blocks: { tag: string; text: string; linkText: string; depth: number; role?: string }[]
      }
      return extractArticle({ title: raw.title, byline: raw.byline, blocks: raw.blocks })
    } catch {
      return null
    }
  }

  // ── omnibox suggestions (frecency: history + bookmarks + open tabs) ──
  suggest(query: string): Suggestion[] {
    return computeSuggestions({
      query,
      history: this.profile.history.entries.map((h) => ({
        url: h.url,
        title: h.title,
        visitCount: h.visitCount,
        lastVisit: h.lastVisit
      })),
      bookmarks: this.profile.bookmarks.nodes
        .filter((n) => Boolean(n.url))
        .map((n) => ({ title: n.title, url: n.url as string })),
      tabs: this.tabs.all.map((t) => {
        const st = t.serialize()
        return { title: st.title, url: st.url }
      })
    })
  }

  // ── clear browsing data ──
  async clearBrowsingData(opts: { history?: boolean; cache?: boolean; storage?: boolean }): Promise<void> {
    if (opts.history) this.profile.history.clear()
    const sessions = this.sessions.sessionsFor(this.profiles.activeId)
    for (const ses of sessions) {
      if (opts.cache) await this.storage.clearCache(ses)
      if (opts.storage) await this.storage.clearStorage(ses)
    }
    this.schedulePush()
  }

  // ── operations ──
  newTab(url?: string, spaceId: string = this.profile.spaces.activeId, activate = true): void {
    const s = this.profile.settings.get()
    const ses = this.acquireSession(this.profiles.activeId, spaceId)
    const tab = this.tabs.create(spaceId, ses, {
      url: url ?? 'about:blank',
      backgroundThrottling: s.tabPolicy !== 'awake',
      activate
    })
    this.hibernation.touch(tab.id)
  }

  navigate(id: string, input: string): void {
    this.tabs.get(id)?.navigate(input, this.profile.settings.get().searchEngine)
    this.hibernation.touch(id)
  }

  setInsets(insets: Insets): void {
    this.layout.setInsets(insets)
    this.layout.position()
  }

  setLayout(state: LayoutState): void {
    this.layoutState = state
    this.relayout()
    this.schedulePush()
  }

  activateSpace(id: string): void {
    this.profile.spaces.activate(id)
    const inSpace = this.tabs.tabsInSpace(id)
    if (inSpace.length > 0) this.tabs.activate(inSpace[0].id)
    else this.newTab(undefined, id, true)
  }

  activateProfile(id: string): void {
    if (id === this.profiles.activeId) return
    this.profiles.activate(id)
    this.bindActiveProfile()
    for (const t of [...this.tabs.all]) this.tabs.close(t.id)
    this.layoutState = { mode: 'single', paneTabIds: [] }
    this.groups = []
    this.newTab()
    this.schedulePush()
  }

  setPermissionPolicy(origin: string, permission: string, decision: PermissionDecision): void {
    this.profile.permissions.setPolicy(origin, permission, decision)
  }

  setExternalPolicy(scheme: string, decision: PermissionDecision): void {
    this.profile.externalProtocols.setPolicy(scheme, decision)
  }

  respondPrompt(id: string, allow: boolean, remember: boolean): void {
    this.prompts.respond(id, allow, remember)
  }

  respondAuth(id: string, creds: AuthCredentials | null): void {
    this.prompts.respondAuth(id, creds)
  }

  // ── chrome web extensions ──
  async loadChromeExtension(dir: string): Promise<ChromeExtensionRecord> {
    const rec = await this.chromeExt.loadUnpacked(dir)
    this.schedulePush()
    return rec
  }
  async setChromeExtensionEnabled(id: string, on: boolean): Promise<void> {
    await this.chromeExt.setEnabled(id, on)
  }
  removeChromeExtension(id: string): void {
    this.chromeExt.remove(id)
  }
  listChromeExtensions(): ChromeExtensionRecord[] {
    return this.chromeExt.list()
  }

  // ── password vault (Sora-User wraps Sora-Master; entries sealed under master) ──
  async createVault(password: string, algo: KdfAlgo = 'argon2id'): Promise<void> { await this.passwords.create(password, algo) }
  async unlockVault(password: string): Promise<boolean> { return this.passwords.unlock(password) }
  lockVault(): void { this.passwords.lock() }
  async changeVaultPassword(oldPw: string, newPw: string): Promise<boolean> { return this.passwords.changePassword(oldPw, newPw) }
  async rotateVault(password: string): Promise<boolean> { return this.passwords.rotate(password) }
  setVaultRotateOnUnlock(on: boolean): void { this.passwords.setRotateOnUnlock(on) }
  addPassword(url: string, cred: Credential): void { this.passwords.addEntry(url, cred); this.schedulePush() }
  updatePassword(id: string, cred: Credential): void { this.passwords.updateEntry(id, cred) }
  removePassword(id: string): void { this.passwords.removeEntry(id) }
  getPassword(id: string): Credential | null { return this.passwords.getCredential(id) }
  matchPasswords(url: string) { return this.passwords.match(url) }
  vaultState(): VaultState { return this.passwords.state() }

  // ── SafeZoning (experimental) ──
  setSafeZoningMode(mode: SafeZoningMode): void { this.safeZoning.setMode(mode) }
  safeZoningState(): SafeZoningState { return this.safeZoning.state() }
  /** Gather page signals, decide, and apply the guard on sensitive pages. */
  async safeZoneTab(id: string): Promise<ZoningDecision | null> {
    if (this.safeZoning.getMode() === 'off') return null
    const tab = this.tabs.get(id)
    if (!tab) return null
    try {
      const wc = tab.ensureView().webContents
      const signals = await wc.executeJavaScript(SIGNALS_SCRIPT, true)
      const decision = this.safeZoning.evaluate(signals)
      if (decision.action !== 'allow') {
        // 'harden' and (scaffolded) 'isolate' both arm the basic guard for now
        await wc.executeJavaScript(this.safeZoning.guardScript(), true)
        this.safeZoning.markZoned(id, true)
      } else {
        this.safeZoning.markZoned(id, false)
      }
      return decision
    } catch { return null }
  }

  downloadStart(url: string): void {
    const wc = this.tabs.getActive()?.ensureView().webContents
    if (wc) this.downloads.triggerViaWebContents(wc, url)
  }
  downloadPause(id: string): void {
    this.downloads.pause(id)
  }
  downloadResume(id: string): void {
    this.downloads.resume(id)
  }
  downloadCancel(id: string): void {
    this.downloads.cancel(id)
  }
  downloadRemove(id: string): void {
    this.downloads.remove(id)
  }
  downloadClear(): void {
    this.downloads.clearCompleted()
  }
  downloadOpen(id: string): void {
    this.downloads.openFile(id)
  }
  downloadShow(id: string): void {
    this.downloads.showInFolder(id)
  }

  // ── session restore ──
  private restoreStore(): SessionRestore {
    return new SessionRestore(this.profile.dir)
  }

  private restoreSession(): void {
    const data = this.restoreStore().load()
    if (!data || data.tabs.length === 0) {
      this.newTab()
      return
    }
    this.profile.spaces.activate(data.activeSpaceId)
    for (const t of data.tabs) this.newTab(t.url, t.spaceId, false)
    const first = this.tabs.tabsInSpace(data.activeSpaceId)[0]
    if (first) this.tabs.activate(first.id)
    else this.newTab()
    this.setLayout(data.layout)
  }

  saveSession(): void {
    if (!this.profile.settings.get().sessionRestore) return
    const data: RestoreData = {
      tabs: this.tabs.all
        .filter((t) => !this.profile.spaces.isPrivate(t.spaceId))
        .map((t) => {
          const s = t.serialize()
          return { spaceId: t.spaceId, url: s.url, title: s.title }
        }),
      activeSpaceId: this.profile.spaces.activeId,
      layout: this.layoutState
    }
    this.restoreStore().save(data)
  }

  // ── sync ──
  private localSnapshot(): SyncableSnapshot {
    return {
      profileId: this.profiles.activeId,
      schema: SYNC_SCHEMA,
      updatedAt: this.dataChangedAt,
      bookmarks: this.profile.bookmarks.nodes,
      settings: this.profile.settings.get()
    }
  }

  private applyRemoteSnapshot(snap: SyncableSnapshot): void {
    this.profile.bookmarks.replaceAll(snap.bookmarks)
    this.profile.settings.patch(snap.settings)
    this.schedulePush()
  }

  // ── state ──
  hello(): Hello {
    return { version: CONTRACT_VERSION, capabilities: this.caps }
  }

  serialize(): BrowserState {
    return {
      capabilities: this.caps,
      runSessionId: RUN_SESSION_ID,
      profiles: this.profiles.all,
      activeProfileId: this.profiles.activeId,
      spaces: this.profile.spaces.all,
      tabs: this.tabs.all.map((t) => t.serialize()),
      activeTabId: this.tabs.activeId,
      activeSpaceId: this.profile.spaces.activeId,
      tabGroups: this.groups,
      layout: this.layoutState,
      settings: this.profile.settings.get(),
      bookmarks: this.profile.bookmarks.nodes,
      extensionContributions: this.profile.extensions.contributions(),
      chromeExtensions: this.chromeExt.list(),
      vault: this.passwords.state(),
      safeZoning: this.safeZoning.state(),
      permissionPolicies: this.profile.permissions.entries,
      externalPolicies: this.profile.externalProtocols.entries,
      pendingPrompts: this.prompts.list(),
      downloads: this.downloads.list(),
      history: this.profile.history.recent(),
      features: this.features.list(this.profile.settings.get()),
      contextMenu: this.pendingContextMenu,
      find: this.findState,
      closedTabs: this.closedTabs,
      zoomPolicies: this.profile.zoom.all(),
      syncStatus: this.sync.status,
      maximized: this.win.isMaximized(),
      platform: HOST_OS
    }
  }

  pushNow(): void {
    this.schedulePush()
  }

  private schedulePush(): void {
    if (this.pushScheduled) return
    this.pushScheduled = true
    queueMicrotask(() => {
      this.pushScheduled = false
      if (!this.chrome.webContents.isDestroyed()) {
        this.chrome.webContents.send(Evt.State, this.serialize())
      }
    })
  }
}
