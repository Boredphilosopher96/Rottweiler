import { EventEmitter } from 'node:events'
import { mergeLWW } from './merge'
import type { SyncAdapter, SyncableSnapshot, SyncStatus } from '@shared/sync'

/**
 * Drives a SyncAdapter with last-write-wins. Pull remote, compare to local by
 * updatedAt, apply whichever is newer, push if local is newer. Predictable and
 * boring on purpose.
 */
export class SyncEngine extends EventEmitter {
  private _status: SyncStatus = 'disabled'

  constructor(
    private adapter: SyncAdapter | null,
    private readonly getLocal: () => SyncableSnapshot,
    private readonly applyRemote: (snap: SyncableSnapshot) => void
  ) {
    super()
    if (adapter) this._status = 'idle'
  }

  get status(): SyncStatus {
    return this._status
  }

  setAdapter(adapter: SyncAdapter | null): void {
    this.adapter = adapter
    this._status = adapter ? 'idle' : 'disabled'
    this.emit('status', this._status)
  }

  async syncNow(): Promise<void> {
    if (!this.adapter) return
    this._status = 'syncing'
    this.emit('status', this._status)
    try {
      const local = this.getLocal()
      const remote = await this.adapter.pull(local.profileId)
      const { winner, apply } = mergeLWW(local, remote)
      if (apply === 'remote' && winner) this.applyRemote(winner)
      else if (apply === 'push-local' && winner) await this.adapter.push(winner)
      this._status = 'idle'
    } catch {
      this._status = 'error'
    }
    this.emit('status', this._status)
  }
}
