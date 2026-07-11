import { ipcMain, type IpcMainEvent } from 'electron'
import { Cmd, HELLO_CHANNEL, SUGGEST_CHANNEL, type CmdPayloads } from '@shared/ipc'
import type { Browser } from '../core/Browser'

/**
 * The one place channels are bound. Commands route through here typed against
 * the shared contract; the hello handshake is the only request/response channel.
 */
export function registerIpc(app: Browser): void {
  function on<C extends keyof CmdPayloads>(ch: C, fn: (payload: CmdPayloads[C]) => void): void {
    ipcMain.on(ch as string, (_e: IpcMainEvent, payload: CmdPayloads[C]) => fn(payload))
  }

  ipcMain.handle(HELLO_CHANNEL, () => app.hello())
  ipcMain.handle(SUGGEST_CHANNEL, (_e, query: string) => app.suggest(query))

  on(Cmd.TabCreate, (p) => app.newTab(p?.url, p?.spaceId, p?.activate ?? true))
  on(Cmd.TabClose, (p) => app.closeTab(p.id))
  on(Cmd.TabActivate, (p) => app.tabs.activate(p.id))
  on(Cmd.TabReorder, (p) => app.tabs.reorder(p.id, p.toIndex))
  on(Cmd.TabNavigate, (p) => app.navigate(p.id, p.input))
  on(Cmd.TabBack, (p) => app.tabs.get(p.id)?.back())
  on(Cmd.TabForward, (p) => app.tabs.get(p.id)?.forward())
  on(Cmd.TabReload, (p) => app.tabs.get(p.id)?.reload())
  on(Cmd.TabStop, (p) => app.tabs.get(p.id)?.stop())
  on(Cmd.TabDuplicate, (p) => app.duplicateTab(p.id))
  on(Cmd.TabSetPinned, (p) => app.setPinned(p.id, p.pinned))
  on(Cmd.TabSetMuted, (p) => app.setMuted(p.id, p.muted))
  on(Cmd.TabDevTools, (p) => app.toggleDevTools(p.id))
  on(Cmd.TabReopenClosed, () => app.reopenClosed())
  on(Cmd.ContextMenuAction, (p) => app.contextMenuAction(p.tabId, p.actionId))
  on(Cmd.ContextMenuClose, () => app.contextMenuClose())
  on(Cmd.FindStart, (p) => app.find(p.id, p.query, { forward: p.forward, matchCase: p.matchCase }))
  on(Cmd.FindNext, (p) => app.findNext(p.id, p.forward))
  on(Cmd.FindStop, (p) => app.findStop(p.id))
  on(Cmd.ZoomIn, (p) => app.zoomIn(p.id))
  on(Cmd.ZoomOut, (p) => app.zoomOut(p.id))
  on(Cmd.ZoomReset, (p) => app.zoomReset(p.id))
  on(Cmd.ZoomSet, (p) => app.zoomSet(p.id, p.factor))

  on(Cmd.SpaceCreate, (p) => app.profile.spaces.create(p.name, p.color, p.private))
  on(Cmd.SpaceActivate, (p) => app.activateSpace(p.id))

  on(Cmd.GroupCreate, (p) => app.createGroup(p.name, p.color))
  on(Cmd.GroupRename, (p) => app.renameGroup(p.id, p.name))
  on(Cmd.GroupSetColor, (p) => app.setGroupColor(p.id, p.color))
  on(Cmd.GroupSetCollapsed, (p) => app.setGroupCollapsed(p.id, p.collapsed))
  on(Cmd.GroupRemove, (p) => app.removeGroup(p.id))
  on(Cmd.TabSetGroup, (p) => app.setTabGroup(p.tabId, p.groupId))
  on(Cmd.ContextMenuReplace, (p) => app.contextMenuReplace(p.tabId, p.word))
  on(Cmd.TabPrintPdf, (p) => void app.printToPdf(p.id))
  on(Cmd.ProfileCreate, (p) => app.profiles.create(p.name, p.color))
  on(Cmd.ProfileActivate, (p) => app.activateProfile(p.id))

  on(Cmd.LayoutSet, (p) => app.setLayout({ mode: p.mode, paneTabIds: p.paneTabIds ?? [] }))
  on(Cmd.LayoutReportInsets, (p) => app.setInsets(p))

  on(Cmd.SettingsPatch, (p) => app.profile.settings.patch(p))

  on(Cmd.BookmarkAdd, (p) => app.profile.bookmarks.add(p.title, p.url, p.parentId ?? null))
  on(Cmd.BookmarkRemove, (p) => app.profile.bookmarks.remove(p.id))
  on(Cmd.BookmarkImport, (p) => app.profile.bookmarks.importHtml(p.html))

  on(Cmd.SyncNow, () => void app.sync.syncNow())

  on(Cmd.ExtSetEnabled, (p) => app.profile.extensions.setEnabled(p.id, p.enabled))

  on(Cmd.DownloadStart, (p) => app.downloadStart(p.url))
  on(Cmd.DownloadPause, (p) => app.downloadPause(p.id))
  on(Cmd.DownloadResume, (p) => app.downloadResume(p.id))
  on(Cmd.DownloadCancel, (p) => app.downloadCancel(p.id))
  on(Cmd.DownloadRemove, (p) => app.downloadRemove(p.id))
  on(Cmd.DownloadClear, () => app.downloadClear())
  on(Cmd.DownloadOpen, (p) => app.downloadOpen(p.id))
  on(Cmd.DownloadShow, (p) => app.downloadShow(p.id))
  on(Cmd.HistoryClear, () => app.clearHistory())
  on(Cmd.ClearBrowsingData, (p) => void app.clearBrowsingData(p))
  on(Cmd.PermissionSet, (p) => app.setPermissionPolicy(p.origin, p.permission, p.decision))
  on(Cmd.ExternalSet, (p) => app.setExternalPolicy(p.scheme, p.decision))
  on(Cmd.PromptRespond, (p) => app.respondPrompt(p.id, p.allow, p.remember))

  on(Cmd.WindowMinimize, () => app.win.minimize())
  on(Cmd.WindowMaximize, () => {
    if (app.win.isMaximized()) app.win.unmaximize()
    else app.win.maximize()
  })
  on(Cmd.WindowClose, () => app.win.close())

  on(Cmd.RequestState, () => app.pushNow())
}
