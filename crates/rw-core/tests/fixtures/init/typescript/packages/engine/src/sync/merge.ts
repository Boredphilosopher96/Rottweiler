import type { SyncableSnapshot } from '@shared/sync'

export const SYNC_SCHEMA = 1

/**
 * Last-write-wins by updatedAt. Deliberately dumb and predictable - the whole
 * point of this design vs a "smart" sync server is that there is nothing to go
 * wonky. Newest snapshot wins wholesale. Returns which side to apply.
 */
export function mergeLWW(
  local: SyncableSnapshot | null,
  remote: SyncableSnapshot | null
): { winner: SyncableSnapshot | null; apply: 'remote' | 'push-local' | 'none' } {
  if (!remote && !local) return { winner: null, apply: 'none' }
  if (!remote) return { winner: local, apply: 'push-local' }
  if (!local) return { winner: remote, apply: 'remote' }

  if (remote.updatedAt > local.updatedAt) return { winner: remote, apply: 'remote' }
  if (local.updatedAt > remote.updatedAt) return { winner: local, apply: 'push-local' }
  return { winner: local, apply: 'none' } // equal timestamps: nothing to do
}
