import { join } from 'node:path'
import { mkdirSync, readFileSync, writeFileSync, existsSync, renameSync } from 'node:fs'
import type { SyncAdapter, SyncableSnapshot } from '@shared/sync'

/**
 * A working sync backend that reads/writes a JSON file in a folder. Point two
 * Sora installs at the same shared/synced folder and they converge. Also the
 * test harness for the sync engine. Real, not a stub.
 */
export class LocalFolderSync implements SyncAdapter {
  readonly id = 'local-folder'

  constructor(private readonly folder: string) {
    mkdirSync(folder, { recursive: true })
  }

  private fileFor(profileId: string): string {
    return join(this.folder, `sora-sync-${profileId}.json`)
  }

  async pull(profileId: string): Promise<SyncableSnapshot | null> {
    const f = this.fileFor(profileId)
    if (!existsSync(f)) return null
    try {
      return JSON.parse(readFileSync(f, 'utf-8')) as SyncableSnapshot
    } catch {
      return null
    }
  }

  async push(snapshot: SyncableSnapshot): Promise<void> {
    const f = this.fileFor(snapshot.profileId)
    const tmp = `${f}.tmp`
    writeFileSync(tmp, JSON.stringify(snapshot, null, 2), 'utf-8')
    renameSync(tmp, f)
  }
}
