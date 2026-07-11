import { SettingsManager } from '../settings/SettingsManager'
import { BookmarkManager } from '../bookmarks/BookmarkManager'
import { SpaceManager } from '../spaces/SpaceManager'
import { ExtensionRegistry } from '../extensions/ExtensionRegistry'
import { PermissionPolicyStore } from '../permissions/PermissionPolicyStore'
import { ExternalPolicyStore } from '../permissions/ExternalPolicyStore'
import { DownloadHistoryStore } from '../downloads/DownloadHistoryStore'
import { HistoryManager } from '../history/HistoryManager'
import { ZoomStore } from '../zoom/ZoomStore'
import type { ProfileMeta } from '@shared/types'

/** Everything owned by one profile: its data dir and per-profile managers. */
export class ProfileContext {
  readonly settings: SettingsManager
  readonly bookmarks: BookmarkManager
  readonly spaces: SpaceManager
  readonly extensions: ExtensionRegistry
  readonly permissions: PermissionPolicyStore
  readonly externalProtocols: ExternalPolicyStore
  readonly downloads: DownloadHistoryStore
  readonly history: HistoryManager
  readonly zoom: ZoomStore

  constructor(
    readonly meta: ProfileMeta,
    readonly dir: string
  ) {
    this.settings = new SettingsManager(dir)
    this.bookmarks = new BookmarkManager(dir)
    this.spaces = new SpaceManager(dir)
    this.extensions = new ExtensionRegistry(dir)
    this.permissions = new PermissionPolicyStore(dir)
    this.externalProtocols = new ExternalPolicyStore(dir)
    this.downloads = new DownloadHistoryStore(dir)
    this.history = new HistoryManager(dir)
    this.zoom = new ZoomStore(dir)
  }
}
