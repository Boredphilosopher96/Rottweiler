export type DownloadState = 'progressing' | 'paused' | 'completed' | 'cancelled' | 'interrupted'

/** How to name a file when one of the same name already exists in the target dir. */
export type CollisionStrategy = 'increment' | 'timestamp' | 'overwrite'

export interface DownloadItemState {
  id: string
  filename: string
  savePath: string
  url: string
  mime: string
  state: DownloadState
  paused: boolean
  receivedBytes: number
  totalBytes: number
  /** smoothed bytes/sec while progressing. */
  speed: number
  /** seconds remaining, or null if unknown. */
  etaSeconds: number | null
  startedAt: number
  completedAt: number | null
  canResume: boolean
  /** extension looks executable/installer-like → UI can warn. */
  dangerous: boolean
}

/** persisted subset for the downloads history list. */
export interface DownloadRecord {
  id: string
  filename: string
  savePath: string
  url: string
  mime: string
  totalBytes: number
  state: DownloadState
  completedAt: number | null
  dangerous: boolean
}
