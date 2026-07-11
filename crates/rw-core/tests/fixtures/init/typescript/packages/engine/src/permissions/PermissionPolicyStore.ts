import { EventEmitter } from 'node:events'
import { Store } from '../persistence/Store'
import { defaultPermission, type PermissionDecision, type PermissionPolicyEntry } from '@shared/permissions'

interface PermData {
  entries: PermissionPolicyEntry[]
}

/** Per-profile, persisted per-origin permission overrides. Emits 'changed'. */
export class PermissionPolicyStore extends EventEmitter {
  private store: Store<PermData>

  constructor(dir: string) {
    super()
    this.store = new Store<PermData>(dir, 'permissions', { entries: [] })
  }

  get entries(): PermissionPolicyEntry[] {
    return this.store.get().entries
  }

  decide(origin: string, permission: string): PermissionDecision {
    const hit = this.entries.find((e) => e.origin === origin && e.permission === permission)
    return hit ? hit.decision : defaultPermission(permission)
  }

  setPolicy(origin: string, permission: string, decision: PermissionDecision): void {
    const entries = this.entries.filter((e) => !(e.origin === origin && e.permission === permission))
    entries.push({ origin, permission, decision })
    this.store.replace({ entries })
    this.emit('changed')
  }

  clear(origin: string): void {
    this.store.replace({ entries: this.entries.filter((e) => e.origin !== origin) })
    this.emit('changed')
  }
}
