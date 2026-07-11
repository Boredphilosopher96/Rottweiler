import { contextBridge, ipcRenderer, type IpcRendererEvent } from 'electron'
import { Cmd, Evt, HELLO_CHANNEL, SUGGEST_CHANNEL, type SoraApi } from '@shared/ipc'
import type { BrowserState } from '@shared/types'
import type { Hello } from '@shared/version'

const api: SoraApi = {
  hello: () => ipcRenderer.invoke(HELLO_CHANNEL) as Promise<Hello>,
  suggest: (query) => ipcRenderer.invoke(SUGGEST_CHANNEL, query),
  tabs: {
    create: (p) => ipcRenderer.send(Cmd.TabCreate, p ?? {}),
    close: (id) => ipcRenderer.send(Cmd.TabClose, { id }),
    activate: (id) => ipcRenderer.send(Cmd.TabActivate, { id }),
    reorder: (id, toIndex) => ipcRenderer.send(Cmd.TabReorder, { id, toIndex }),
    navigate: (id, input) => ipcRenderer.send(Cmd.TabNavigate, { id, input }),
    back: (id) => ipcRenderer.send(Cmd.TabBack, { id }),
    forward: (id) => ipcRenderer.send(Cmd.TabForward, { id }),
    reload: (id) => ipcRenderer.send(Cmd.TabReload, { id }),
    stop: (id) => ipcRenderer.send(Cmd.TabStop, { id }),
    duplicate: (id) => ipcRenderer.send(Cmd.TabDuplicate, { id }),
    setPinned: (id, pinned) => ipcRenderer.send(Cmd.TabSetPinned, { id, pinned }),
    setMuted: (id, muted) => ipcRenderer.send(Cmd.TabSetMuted, { id, muted }),
    devtools: (id) => ipcRenderer.send(Cmd.TabDevTools, { id }),
    reopenClosed: () => ipcRenderer.send(Cmd.TabReopenClosed),
    setGroup: (tabId, groupId) => ipcRenderer.send(Cmd.TabSetGroup, { tabId, groupId }),
    printPdf: (id) => ipcRenderer.send(Cmd.TabPrintPdf, { id })
  },
  groups: {
    create: (name, color) => ipcRenderer.send(Cmd.GroupCreate, { name, color }),
    rename: (id, name) => ipcRenderer.send(Cmd.GroupRename, { id, name }),
    setColor: (id, color) => ipcRenderer.send(Cmd.GroupSetColor, { id, color }),
    setCollapsed: (id, collapsed) => ipcRenderer.send(Cmd.GroupSetCollapsed, { id, collapsed }),
    remove: (id) => ipcRenderer.send(Cmd.GroupRemove, { id })
  },
  contextMenu: {
    action: (tabId, actionId) => ipcRenderer.send(Cmd.ContextMenuAction, { tabId, actionId }),
    replace: (tabId, word) => ipcRenderer.send(Cmd.ContextMenuReplace, { tabId, word }),
    close: () => ipcRenderer.send(Cmd.ContextMenuClose)
  },
  find: {
    start: (id, query, opts) => ipcRenderer.send(Cmd.FindStart, { id, query, ...(opts ?? {}) }),
    next: (id, forward) => ipcRenderer.send(Cmd.FindNext, { id, forward }),
    stop: (id) => ipcRenderer.send(Cmd.FindStop, { id })
  },
  zoom: {
    in: (id) => ipcRenderer.send(Cmd.ZoomIn, { id }),
    out: (id) => ipcRenderer.send(Cmd.ZoomOut, { id }),
    reset: (id) => ipcRenderer.send(Cmd.ZoomReset, { id }),
    set: (id, factor) => ipcRenderer.send(Cmd.ZoomSet, { id, factor })
  },
  spaces: {
    create: (name, color, isPrivate) => ipcRenderer.send(Cmd.SpaceCreate, { name, color, private: isPrivate }),
    activate: (id) => ipcRenderer.send(Cmd.SpaceActivate, { id })
  },
  profiles: {
    create: (name, color) => ipcRenderer.send(Cmd.ProfileCreate, { name, color }),
    activate: (id) => ipcRenderer.send(Cmd.ProfileActivate, { id })
  },
  layout: {
    set: (mode, paneTabIds) => ipcRenderer.send(Cmd.LayoutSet, { mode, paneTabIds }),
    reportInsets: (insets) => ipcRenderer.send(Cmd.LayoutReportInsets, insets)
  },
  settings: {
    patch: (patch) => ipcRenderer.send(Cmd.SettingsPatch, patch)
  },
  bookmarks: {
    add: (title, url, parentId) => ipcRenderer.send(Cmd.BookmarkAdd, { title, url, parentId }),
    remove: (id) => ipcRenderer.send(Cmd.BookmarkRemove, { id }),
    importHtml: (html) => ipcRenderer.send(Cmd.BookmarkImport, { html })
  },
  sync: {
    now: () => ipcRenderer.send(Cmd.SyncNow)
  },
  extensions: {
    setEnabled: (id, enabled) => ipcRenderer.send(Cmd.ExtSetEnabled, { id, enabled })
  },
  history: {
    clear: () => ipcRenderer.send(Cmd.HistoryClear)
  },
  data: {
    clear: (opts) => ipcRenderer.send(Cmd.ClearBrowsingData, opts)
  },
  downloads: {
    start: (url) => ipcRenderer.send(Cmd.DownloadStart, { url }),
    pause: (id) => ipcRenderer.send(Cmd.DownloadPause, { id }),
    resume: (id) => ipcRenderer.send(Cmd.DownloadResume, { id }),
    cancel: (id) => ipcRenderer.send(Cmd.DownloadCancel, { id }),
    remove: (id) => ipcRenderer.send(Cmd.DownloadRemove, { id }),
    clear: () => ipcRenderer.send(Cmd.DownloadClear),
    open: (id) => ipcRenderer.send(Cmd.DownloadOpen, { id }),
    showInFolder: (id) => ipcRenderer.send(Cmd.DownloadShow, { id })
  },
  permissions: {
    setPolicy: (origin, permission, decision) =>
      ipcRenderer.send(Cmd.PermissionSet, { origin, permission, decision }),
    setExternalPolicy: (scheme, decision) => ipcRenderer.send(Cmd.ExternalSet, { scheme, decision }),
    respond: (id, allow, remember) => ipcRenderer.send(Cmd.PromptRespond, { id, allow, remember })
  },
  window: {
    minimize: () => ipcRenderer.send(Cmd.WindowMinimize),
    maximize: () => ipcRenderer.send(Cmd.WindowMaximize),
    close: () => ipcRenderer.send(Cmd.WindowClose)
  },
  requestState: () => ipcRenderer.send(Cmd.RequestState),
  onState: (cb: (s: BrowserState) => void) => {
    const listener = (_e: IpcRendererEvent, s: BrowserState): void => cb(s)
    ipcRenderer.on(Evt.State, listener)
    return () => {
      ipcRenderer.removeListener(Evt.State, listener)
    }
  }
}

contextBridge.exposeInMainWorld('sora', api)
