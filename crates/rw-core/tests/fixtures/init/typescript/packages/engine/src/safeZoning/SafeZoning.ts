import { EventEmitter } from 'node:events'
import { decideZoning } from './policy'
import { BASIC_GUARD_SCRIPT } from './guardScript'
import type { SafeZoningMode, PageSignals, ZoningDecision, SafeZoningState } from '../contract/safeZoning'

/**
 * Experimental Sora-side hardening for sensitive pages. Holds the mode and,
 * per page, decides whether to leave it alone, harden it (inject the guard), or
 * isolate it (forced — the isolated-context flow is scaffolded). Pure decisions
 * live in ./policy; this class is the stateful coordinator.
 */
export class SafeZoning extends EventEmitter {
  private mode: SafeZoningMode = 'off'
  private zones = new Set<string>()
  private lastDecision: ZoningDecision | null = null

  getMode(): SafeZoningMode { return this.mode }
  setMode(mode: SafeZoningMode): void {
    if (mode === this.mode) return
    this.mode = mode
    if (mode === 'off') this.zones.clear()
    this.emit('changed')
  }

  /** Decide what to do with a page and record it. The caller applies the action. */
  evaluate(signals: PageSignals): ZoningDecision {
    const decision = decideZoning(this.mode, signals)
    this.lastDecision = decision
    this.emit('changed')
    return decision
  }

  markZoned(tabId: string, on: boolean): void {
    if (on) this.zones.add(tabId); else this.zones.delete(tabId)
    this.emit('changed')
  }

  guardScript(): string { return BASIC_GUARD_SCRIPT }

  state(): SafeZoningState {
    return { mode: this.mode, experimental: true, activeZones: this.zones.size, lastDecision: this.lastDecision }
  }
}
