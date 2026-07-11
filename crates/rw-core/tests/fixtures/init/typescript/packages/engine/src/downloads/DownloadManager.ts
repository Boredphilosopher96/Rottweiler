import { app, shell, type Session, type DownloadItem } from 'electron'
import { join } from 'node:path'
import { existsSync } from 'node:fs'
import { randomUUID } from 'node:crypto'
import { resolveFilename, sanitizeFilename, isDangerous } from './paths'
import type { DownloadHistoryStore } from './DownloadHistoryStore'
import type { SoraSettings } from '@shared/settings'
import type { DownloadItemState, DownloadRecord, DownloadState } from '@shared/downloads'

interface Live {
  item: DownloadItem
  state: DownloadItemState
  lastBytes: number
  lastTime: number
}

interface Deps {
  getSettings: () => SoraSettings
  getHistory: () => DownloadHistoryStore
  onChange: () => void
}

/**
 * Runtime download manager for a window. Attaches to each session's will-download,
 * resolves save paths (respecting the user's collision strategy + prompt setting),
 * tracks progress/speed/ETA, supports pause/resume/cancel/open/show/retry, and
 * persists completed downloads to the active profile's history.
 */
export class DownloadManager {
  private live = new Map<string, Live>()
  private attached = new WeakSet<Session>()

  constructor(private readonly deps: Deps) {}

  attach(session: Session): void {
    if (this.attached.has(session)) return
    this.attached.add(session)
    session.on('will-download', (_event, item) => this.onWillDownload(item))
  }

  /** Start a download from a URL (Save link as / retry). */
  triggerViaWebContents(wc: Electron.WebContents, url: string): void {
    wc.downloadURL(url)
  }

  private downloadsDir(): string {
    const dir = this.deps.getSettings().downloadDir
    return dir && dir.length > 0 ? dir : app.getPath('downloads')
  }

  private onWillDownload(item: DownloadItem): void {
    const settings = this.deps.getSettings()
    const rawName = item.getFilename()
    const clean = sanitizeFilename(rawName)

    if (!settings.downloadPromptForLocation) {
      const dir = this.downloadsDir()
      const finalName = resolveFilename(clean, settings.downloadCollision, (n) => existsSync(join(dir, n)))
      item.setSavePath(join(dir, finalName))
    }
    // else: leave savePath unset → Electron shows its native Save dialog.

    const id = randomUUID()
    const state: DownloadItemState = {
      id,
      filename: clean,
      savePath: item.getSavePath(),
      url: item.getURL(),
      mime: item.getMimeType(),
      state: 'progressing',
      paused: false,
      receivedBytes: 0,
      totalBytes: item.getTotalBytes(),
      speed: 0,
      etaSeconds: null,
      startedAt: Date.now(),
      completedAt: null,
      canResume: false,
      dangerous: isDangerous(clean)
    }
    const rec: Live = { item, state, lastBytes: 0, lastTime: Date.now() }
    this.live.set(id, rec)

    item.on('updated', (_e, s) => this.onUpdated(rec, s))
    item.on('done', (_e, s) => this.onDone(rec, s))
    this.deps.onChange()
  }

  private onUpdated(rec: Live, s: 'progressing' | 'interrupted'): void {
    const { item, state } = rec
    const now = Date.now()
    const received = item.getReceivedBytes()
    const dt = (now - rec.lastTime) / 1000
    if (dt >= 0.4) {
      const inst = (received - rec.lastBytes) / dt
      state.speed = state.speed === 0 ? inst : state.speed * 0.6 + inst * 0.4
      rec.lastBytes = received
      rec.lastTime = now
    }
    state.receivedBytes = received
    state.totalBytes = item.getTotalBytes()
    state.paused = item.isPaused()
    state.savePath = item.getSavePath()
    state.state = s === 'interrupted' ? 'interrupted' : item.isPaused() ? 'paused' : 'progressing'
    state.canResume = item.canResume()
    const remain = state.totalBytes - received
    state.etaSeconds = state.speed > 0 && remain > 0 ? Math.round(remain / state.speed) : null
    this.deps.onChange()
  }

  private onDone(rec: Live, s: 'completed' | 'cancelled' | 'interrupted'): void {
    const { state } = rec
    state.state = s as DownloadState
    state.paused = false
    state.speed = 0
    state.etaSeconds = null
    state.completedAt = Date.now()
    state.savePath = rec.item.getSavePath()
    if (s === 'completed') state.receivedBytes = state.totalBytes
    this.persist(state)
    this.deps.onChange()
  }

  private persist(state: DownloadItemState): void {
    const rec: DownloadRecord = {
      id: state.id,
      filename: state.filename,
      savePath: state.savePath,
      url: state.url,
      mime: state.mime,
      totalBytes: state.totalBytes,
      state: state.state,
      completedAt: state.completedAt,
      dangerous: state.dangerous
    }
    this.deps.getHistory().upsert(rec)
  }

  // ── ops ──
  pause(id: string): void {
    this.live.get(id)?.item.pause()
    this.deps.onChange()
  }
  resume(id: string): void {
    const l = this.live.get(id)
    if (l && l.item.canResume()) l.item.resume()
    this.deps.onChange()
  }
  cancel(id: string): void {
    this.live.get(id)?.item.cancel()
    this.deps.onChange()
  }
  remove(id: string): void {
    this.live.delete(id)
    this.deps.getHistory().remove(id)
    this.deps.onChange()
  }
  clearCompleted(): void {
    for (const [id, l] of [...this.live]) if (l.state.completedAt) this.live.delete(id)
    this.deps.getHistory().clear()
    this.deps.onChange()
  }
  openFile(id: string): void {
    const path = this.pathFor(id)
    if (path) void shell.openPath(path)
  }
  showInFolder(id: string): void {
    const path = this.pathFor(id)
    if (path) shell.showItemInFolder(path)
  }

  private pathFor(id: string): string | null {
    const l = this.live.get(id)
    if (l) return l.state.savePath
    return this.deps.getHistory().items.find((r) => r.id === id)?.savePath ?? null
  }

  /** live items first (newest first), then persisted history not currently live. */
  list(): DownloadItemState[] {
    const liveStates = [...this.live.values()].map((l) => l.state).sort((a, b) => b.startedAt - a.startedAt)
    const liveIds = new Set(liveStates.map((s) => s.id))
    const historic: DownloadItemState[] = this.deps
      .getHistory()
      .items.filter((r) => !liveIds.has(r.id))
      .map((r) => ({
        id: r.id,
        filename: r.filename,
        savePath: r.savePath,
        url: r.url,
        mime: r.mime,
        state: r.state,
        paused: false,
        receivedBytes: r.totalBytes,
        totalBytes: r.totalBytes,
        speed: 0,
        etaSeconds: null,
        startedAt: r.completedAt ?? 0,
        completedAt: r.completedAt,
        canResume: false,
        dangerous: r.dangerous
      }))
    return [...liveStates, ...historic]
  }
}
