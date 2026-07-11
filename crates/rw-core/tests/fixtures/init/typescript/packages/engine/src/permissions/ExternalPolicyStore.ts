import { EventEmitter } from 'node:events'
import { Store } from '../persistence/Store'
import type { ExternalPolicyEntry, PermissionDecision } from '@shared/permissions'

interface ExtData {
  entries: ExternalPolicyEntry[]
}

/** Per-profile, persisted decisions for opening external app schemes. */
export class ExternalPolicyStore extends EventEmitter {
  private store: Store<ExtData>

  constructor(dir: string) {
    super()
    this.store = new Store<ExtData>(dir, 'external-protocols', { entries: [] })
  }

  get entries(): ExternalPolicyEntry[] {
    return this.store.get().entries
  }

  decide(scheme: string): PermissionDecision {
    const hit = this.entries.find((e) => e.scheme === scheme)
    return hit ? hit.decision : 'ask'
  }

  setPolicy(scheme: string, decision: PermissionDecision): void {
    const entries = this.entries.filter((e) => e.scheme !== scheme)
    entries.push({ scheme, decision })
    this.store.replace({ entries })
    this.emit('changed')
  }
}
