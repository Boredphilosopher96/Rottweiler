import type { SoraSettings } from './settings'
import type { BookmarkNode } from './bookmarks'
import type { Contributions } from './extensions'
import type { Capabilities } from './version'
import type { OSInfo } from './platform'
import type { SyncStatus } from './sync'
import type { PermissionPolicyEntry, ExternalPolicyEntry, PendingPrompt } from './permissions'
import type { SecurityInfo } from './security'
import type { ChromeExtensionRecord } from './chromeExtensions'
import type { VaultState } from './vault'
import type { SafeZoningState } from './safeZoning'
import type { DownloadItemState } from './downloads'
import type { HistoryEntry } from './history'
import type { FeatureInfo } from './features'
import type { ContextMenuModel } from './menus'
import type { FindState } from './find'
import type { ZoomLevel } from './zoom'

// The authoritative state shapes. The main process owns instances of these;
// the UI only ever receives snapshots and renders them. Never mutated in the renderer.

export interface TabState {
  id: string
  spaceId: string
  url: string
  title: string
  favicon: string | null
  loading: boolean
  canGoBack: boolean
  canGoForward: boolean
  /** true when the tab's view has been discarded to save memory. */
  hibernated: boolean
  zoom: number
  pinned: boolean
  muted: boolean
  audible: boolean
  /** true when the current page is served over https. */
  secure: boolean
  /** full security posture for the badge + detail panel */
  security: SecurityInfo
  /** id of the tab group this tab belongs to, or null. */
  groupId: string | null
}

export interface TabGroupState {
  id: string
  name: string
  color: string
  collapsed: boolean
}

export interface ClosedTab {
  url: string
  title: string
  spaceId: string
  closedAt: number
}

export interface SpaceState {
  id: string
  name: string
  color: string
  /** private spaces use an in-memory session and record no history. */
  private: boolean
}

export interface ProfileMeta {
  id: string
  name: string
  color: string
}

export type LayoutMode = 'single' | 'split'

export interface LayoutState {
  mode: LayoutMode
  paneTabIds: string[]
}

export interface Insets {
  top: number
  left: number
  right: number
  bottom: number
}

/** The single snapshot pushed to the UI on any change. */
export interface BrowserState {
  capabilities: Capabilities
  runSessionId: string
  profiles: ProfileMeta[]
  activeProfileId: string
  spaces: SpaceState[]
  tabs: TabState[]
  activeTabId: string | null
  activeSpaceId: string
  tabGroups: TabGroupState[]
  layout: LayoutState
  settings: SoraSettings
  bookmarks: BookmarkNode[]
  extensionContributions: Contributions
  chromeExtensions: ChromeExtensionRecord[]
  vault: VaultState
  safeZoning: SafeZoningState
  permissionPolicies: PermissionPolicyEntry[]
  externalPolicies: ExternalPolicyEntry[]
  pendingPrompts: PendingPrompt[]
  downloads: DownloadItemState[]
  history: HistoryEntry[]
  features: FeatureInfo[]
  contextMenu: ContextMenuModel | null
  find: FindState | null
  closedTabs: ClosedTab[]
  zoomPolicies: ZoomLevel[]
  syncStatus: SyncStatus
  maximized: boolean
  platform: OSInfo
}
