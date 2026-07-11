import type { BrowserState, Insets, LayoutMode } from './types'
import type { SoraSettings } from './settings'
import type { Hello } from './version'
import type { PermissionDecision } from './permissions'
import type { Suggestion } from './omnibox'

// ── Command channels (UI → engine). Fire-and-forget; state always returns via the
// STATE push, never as a command result, so the UI has one source of truth. ──
export const Cmd = {
  TabCreate: 'tab.create',
  TabClose: 'tab.close',
  TabActivate: 'tab.activate',
  TabReorder: 'tab.reorder',
  TabNavigate: 'tab.navigate',
  TabBack: 'tab.back',
  TabForward: 'tab.forward',
  TabReload: 'tab.reload',
  TabStop: 'tab.stop',
  TabDuplicate: 'tab.duplicate',
  TabSetPinned: 'tab.setPinned',
  TabSetMuted: 'tab.setMuted',
  TabDevTools: 'tab.devtools',
  TabReopenClosed: 'tab.reopenClosed',
  ContextMenuAction: 'ctx.action',
  ContextMenuClose: 'ctx.close',
  FindStart: 'find.start',
  FindNext: 'find.next',
  FindStop: 'find.stop',
  ZoomIn: 'zoom.in',
  ZoomOut: 'zoom.out',
  ZoomReset: 'zoom.reset',
  ZoomSet: 'zoom.set',
  SpaceCreate: 'space.create',
  SpaceActivate: 'space.activate',
  GroupCreate: 'group.create',
  GroupRename: 'group.rename',
  GroupSetColor: 'group.setColor',
  GroupSetCollapsed: 'group.setCollapsed',
  GroupRemove: 'group.remove',
  TabSetGroup: 'tab.setGroup',
  ContextMenuReplace: 'ctx.replace',
  TabPrintPdf: 'tab.printPdf',
  ProfileCreate: 'profile.create',
  ProfileActivate: 'profile.activate',
  LayoutSet: 'layout.set',
  LayoutReportInsets: 'layout.reportInsets',
  SettingsPatch: 'settings.patch',
  BookmarkAdd: 'bookmark.add',
  BookmarkRemove: 'bookmark.remove',
  BookmarkImport: 'bookmark.import',
  SyncNow: 'sync.now',
  ExtSetEnabled: 'ext.setEnabled',
  PermissionSet: 'perm.set',
  ExternalSet: 'perm.external.set',
  PromptRespond: 'prompt.respond',
  DownloadStart: 'dl.start',
  DownloadPause: 'dl.pause',
  DownloadResume: 'dl.resume',
  DownloadCancel: 'dl.cancel',
  DownloadRemove: 'dl.remove',
  DownloadClear: 'dl.clear',
  DownloadOpen: 'dl.open',
  DownloadShow: 'dl.show',
  HistoryClear: 'history.clear',
  ClearBrowsingData: 'data.clear',
  WindowMinimize: 'window.minimize',
  WindowMaximize: 'window.maximize',
  WindowClose: 'window.close',
  RequestState: 'state.request'
} as const

export type CmdChannel = (typeof Cmd)[keyof typeof Cmd]

export interface CmdPayloads {
  [Cmd.TabCreate]: { url?: string; spaceId?: string; activate?: boolean }
  [Cmd.TabClose]: { id: string }
  [Cmd.TabActivate]: { id: string }
  [Cmd.TabReorder]: { id: string; toIndex: number }
  [Cmd.TabNavigate]: { id: string; input: string }
  [Cmd.TabBack]: { id: string }
  [Cmd.TabForward]: { id: string }
  [Cmd.TabReload]: { id: string }
  [Cmd.TabStop]: { id: string }
  [Cmd.TabDuplicate]: { id: string }
  [Cmd.TabSetPinned]: { id: string; pinned: boolean }
  [Cmd.TabSetMuted]: { id: string; muted: boolean }
  [Cmd.TabDevTools]: { id: string }
  [Cmd.TabReopenClosed]: void
  [Cmd.ContextMenuAction]: { tabId: string; actionId: string }
  [Cmd.ContextMenuClose]: void
  [Cmd.FindStart]: { id: string; query: string; forward?: boolean; matchCase?: boolean }
  [Cmd.FindNext]: { id: string; forward: boolean }
  [Cmd.FindStop]: { id: string }
  [Cmd.ZoomIn]: { id: string }
  [Cmd.ZoomOut]: { id: string }
  [Cmd.ZoomReset]: { id: string }
  [Cmd.ZoomSet]: { id: string; factor: number }
  [Cmd.SpaceCreate]: { name: string; color: string; private?: boolean }
  [Cmd.SpaceActivate]: { id: string }
  [Cmd.ProfileCreate]: { name: string; color: string }
  [Cmd.ProfileActivate]: { id: string }
  [Cmd.GroupCreate]: { name: string; color: string }
  [Cmd.GroupRename]: { id: string; name: string }
  [Cmd.GroupSetColor]: { id: string; color: string }
  [Cmd.GroupSetCollapsed]: { id: string; collapsed: boolean }
  [Cmd.GroupRemove]: { id: string }
  [Cmd.TabSetGroup]: { tabId: string; groupId: string | null }
  [Cmd.ContextMenuReplace]: { tabId: string; word: string }
  [Cmd.TabPrintPdf]: { id: string }
  [Cmd.LayoutSet]: { mode: LayoutMode; paneTabIds?: string[] }
  [Cmd.LayoutReportInsets]: Insets
  [Cmd.SettingsPatch]: Partial<SoraSettings>
  [Cmd.BookmarkAdd]: { title: string; url: string; parentId?: string | null }
  [Cmd.BookmarkRemove]: { id: string }
  [Cmd.BookmarkImport]: { html: string }
  [Cmd.SyncNow]: void
  [Cmd.ExtSetEnabled]: { id: string; enabled: boolean }
  [Cmd.PermissionSet]: { origin: string; permission: string; decision: PermissionDecision }
  [Cmd.ExternalSet]: { scheme: string; decision: PermissionDecision }
  [Cmd.PromptRespond]: { id: string; allow: boolean; remember: boolean }
  [Cmd.DownloadStart]: { url: string }
  [Cmd.DownloadPause]: { id: string }
  [Cmd.DownloadResume]: { id: string }
  [Cmd.DownloadCancel]: { id: string }
  [Cmd.DownloadRemove]: { id: string }
  [Cmd.DownloadClear]: void
  [Cmd.DownloadOpen]: { id: string }
  [Cmd.DownloadShow]: { id: string }
  [Cmd.HistoryClear]: void
  [Cmd.ClearBrowsingData]: { history?: boolean; cache?: boolean; storage?: boolean }
  [Cmd.WindowMinimize]: void
  [Cmd.WindowMaximize]: void
  [Cmd.WindowClose]: void
  [Cmd.RequestState]: void
}

// ── Event channels (engine → UI) ──
export const Evt = { State: 'state' } as const
export interface EvtPayloads {
  [Evt.State]: BrowserState
}

// ── The typed API surface exposed on window.sora by the preload. ──
export interface SoraApi {
  /** ABI handshake: version + capabilities. Call on boot to negotiate. */
  hello(): Promise<Hello>
  /** omnibox: frecency-ranked suggestions from history, bookmarks and open tabs. */
  suggest(query: string): Promise<Suggestion[]>
  tabs: {
    create(payload?: CmdPayloads[typeof Cmd.TabCreate]): void
    close(id: string): void
    activate(id: string): void
    reorder(id: string, toIndex: number): void
    navigate(id: string, input: string): void
    back(id: string): void
    forward(id: string): void
    reload(id: string): void
    stop(id: string): void
    duplicate(id: string): void
    setPinned(id: string, pinned: boolean): void
    setMuted(id: string, muted: boolean): void
    devtools(id: string): void
    reopenClosed(): void
    setGroup(tabId: string, groupId: string | null): void
    printPdf(id: string): void
  }
  groups: {
    create(name: string, color: string): void
    rename(id: string, name: string): void
    setColor(id: string, color: string): void
    setCollapsed(id: string, collapsed: boolean): void
    remove(id: string): void
  }
  contextMenu: {
    action(tabId: string, actionId: string): void
    replace(tabId: string, word: string): void
    close(): void
  }
  find: {
    start(id: string, query: string, opts?: { forward?: boolean; matchCase?: boolean }): void
    next(id: string, forward: boolean): void
    stop(id: string): void
  }
  zoom: {
    in(id: string): void
    out(id: string): void
    reset(id: string): void
    set(id: string, factor: number): void
  }
  spaces: {
    create(name: string, color: string, isPrivate?: boolean): void
    activate(id: string): void
  }
  profiles: {
    create(name: string, color: string): void
    activate(id: string): void
  }
  layout: {
    set(mode: LayoutMode, paneTabIds?: string[]): void
    reportInsets(insets: Insets): void
  }
  settings: {
    patch(patch: Partial<SoraSettings>): void
  }
  bookmarks: {
    add(title: string, url: string, parentId?: string | null): void
    remove(id: string): void
    importHtml(html: string): void
  }
  sync: {
    now(): void
  }
  extensions: {
    setEnabled(id: string, enabled: boolean): void
  }
  permissions: {
    setPolicy(origin: string, permission: string, decision: PermissionDecision): void
    setExternalPolicy(scheme: string, decision: PermissionDecision): void
    respond(promptId: string, allow: boolean, remember: boolean): void
  }
  history: {
    clear(): void
  }
  data: {
    clear(opts: { history?: boolean; cache?: boolean; storage?: boolean }): void
  }
  downloads: {
    start(url: string): void
    pause(id: string): void
    resume(id: string): void
    cancel(id: string): void
    remove(id: string): void
    clear(): void
    open(id: string): void
    showInFolder(id: string): void
  }
  window: {
    minimize(): void
    maximize(): void
    close(): void
  }
  requestState(): void
  onState(cb: (state: BrowserState) => void): () => void
}


export const HELLO_CHANNEL = 'sora:hello'
export const SUGGEST_CHANNEL = 'sora:suggest'
