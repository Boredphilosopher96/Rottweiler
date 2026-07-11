import type { TabPolicy } from '@shared/settings'
import type { Tab } from './Tab'

interface HibernationDeps {
  getTabs: () => Tab[]
  /** ids that must never sleep: active tab + any split panes. */
  getProtectedIds: () => string[]
  getPolicy: () => { policy: TabPolicy; minutes: number }
  onChange: () => void
}

/**
 * Enforces the tab policy. 'sleep' discards inactive tabs past a threshold to
 * reclaim memory; 'awake' never discards and turns background throttling off so
 * network/timers stay live at the cost of RAM. The user chooses in settings.
 */
export class HibernationManager {
  private lastActive = new Map<string, number>()
  private timer: NodeJS.Timeout | null = null

  constructor(private readonly deps: HibernationDeps) {}

  start(): void {
    if (this.timer) return
    this.timer = setInterval(() => this.tick(), 60_000)
    this.applyThrottling()
  }

  stop(): void {
    if (this.timer) clearInterval(this.timer)
    this.timer = null
  }

  touch(id: string): void {
    this.lastActive.set(id, Date.now())
  }

  /** re-apply throttling to all tabs after a policy change. */
  applyThrottling(): void {
    const awake = this.deps.getPolicy().policy === 'awake'
    for (const t of this.deps.getTabs()) t.setBackgroundThrottling(!awake)
    if (awake) void this.wakeAll()
  }

  private async wakeAll(): Promise<void> {
    for (const t of this.deps.getTabs()) if (t.hibernated) t.wake()
    this.deps.onChange()
  }

  private tick(): void {
    const { policy, minutes } = this.deps.getPolicy()
    if (policy !== 'sleep') return
    const cutoff = Date.now() - minutes * 60_000
    const protectedIds = new Set(this.deps.getProtectedIds())
    let changed = false
    for (const t of this.deps.getTabs()) {
      if (t.hibernated || protectedIds.has(t.id)) continue
      const last = this.lastActive.get(t.id) ?? 0
      if (last < cutoff) {
        void t.hibernate()
        changed = true
      }
    }
    if (changed) this.deps.onChange()
  }

  /** test/manual hook: hibernate a specific inactive tab now. */
  async hibernateNow(t: Tab): Promise<void> {
    await t.hibernate()
    this.deps.onChange()
  }
}
