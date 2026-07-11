import type { BookmarkNode } from './bookmarks'
import type { SoraSettings } from './settings'

/** The portable subset of a profile that syncs. Reading list etc join later. */
export interface SyncableSnapshot {
  profileId: string
  /** schema version of the snapshot payload itself. */
  schema: number
  /** ms epoch of last local change; drives last-write-wins. */
  updatedAt: number
  bookmarks: BookmarkNode[]
  settings: SoraSettings
}

/** A sync backend. Implementations: LocalFolder (works now), GoogleDrive (needs owner OAuth). */
export interface SyncAdapter {
  readonly id: string
  pull(profileId: string): Promise<SyncableSnapshot | null>
  push(snapshot: SyncableSnapshot): Promise<void>
}

export type SyncStatus = 'idle' | 'syncing' | 'error' | 'disabled'
