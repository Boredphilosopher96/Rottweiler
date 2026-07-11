import { EventEmitter } from 'node:events'
import { randomUUID } from 'node:crypto'
import type { PendingPrompt, AuthCredentials } from '@shared/permissions'

type Resolver = (allow: boolean, remember: boolean) => void
type AuthResolver = (creds: AuthCredentials | null) => void

/**
 * Runtime queue of prompts awaiting a user answer (per window). Site permission
 * requests and external-app requests both land here; the UI renders them from
 * state and answers via respond(). Unanswered prompts auto-deny after a timeout.
 */
export class PromptQueue extends EventEmitter {
  private pending = new Map<string, { prompt: PendingPrompt; resolve?: Resolver; authResolve?: AuthResolver; timer: NodeJS.Timeout }>()

  add(base: Omit<PendingPrompt, 'id'>, resolve: Resolver, timeoutMs = 45_000): string {
    const id = randomUUID()
    const prompt: PendingPrompt = { id, ...base }
    const timer = setTimeout(() => this.respond(id, false, false), timeoutMs)
    this.pending.set(id, { prompt, resolve, timer })
    this.emit('changed')
    return id
  }

  /** Queue an httpAuth prompt resolved with credentials (or null to cancel). */
  addAuth(base: Omit<PendingPrompt, 'id'>, resolve: AuthResolver, timeoutMs = 90_000): string {
    const id = randomUUID()
    const prompt: PendingPrompt = { id, ...base }
    const timer = setTimeout(() => this.respondAuth(id, null), timeoutMs)
    this.pending.set(id, { prompt, authResolve: resolve, timer })
    this.emit('changed')
    return id
  }

  /** Answer an httpAuth prompt. Pass null (or nothing) to cancel the auth. */
  respondAuth(id: string, creds: AuthCredentials | null): void {
    const entry = this.pending.get(id)
    if (!entry) return
    clearTimeout(entry.timer)
    this.pending.delete(id)
    entry.authResolve?.(creds)
    this.emit('changed')
  }

  list(): PendingPrompt[] {
    return [...this.pending.values()].map((p) => p.prompt)
  }

  respond(id: string, allow: boolean, remember: boolean): void {
    const entry = this.pending.get(id)
    if (!entry) return
    clearTimeout(entry.timer)
    this.pending.delete(id)
    if (entry.authResolve) entry.authResolve(null)
    else entry.resolve?.(allow, remember)
    this.emit('changed')
  }
}
