// Pure bookkeeping for multi-window support: tracks open windows, focus order,
// and per-window tab counts. The WindowManager (Electron) owns real BaseWindows
// and one Browser each; this registry is the testable state it drives.

export interface WindowMeta {
  id: string
  createdAt: number
  focused: boolean
  tabCount: number
  lastFocusedAt: number
}

export class WindowRegistry {
  private wins = new Map<string, WindowMeta>()
  private focusedId: string | null = null
  private clock = 0

  open(id: string, now: number = Date.now()): WindowMeta {
    if (this.wins.has(id)) return this.wins.get(id)!
    const meta: WindowMeta = { id, createdAt: now, focused: false, tabCount: 0, lastFocusedAt: ++this.clock }
    this.wins.set(id, meta)
    this.focus(id)
    return meta
  }

  close(id: string): void {
    if (!this.wins.delete(id)) return
    if (this.focusedId === id) {
      // hand focus to the most-recently-focused remaining window
      let next: WindowMeta | null = null
      for (const w of this.wins.values()) if (!next || w.lastFocusedAt > next.lastFocusedAt) next = w
      this.focusedId = next?.id ?? null
      if (next) next.focused = true
    }
  }

  focus(id: string): void {
    const w = this.wins.get(id)
    if (!w) return
    for (const other of this.wins.values()) other.focused = false
    w.focused = true
    w.lastFocusedAt = ++this.clock
    this.focusedId = id
  }

  setTabCount(id: string, n: number): void {
    const w = this.wins.get(id)
    if (w) w.tabCount = Math.max(0, n)
  }

  focused(): string | null { return this.focusedId }
  get(id: string): WindowMeta | undefined { return this.wins.get(id) }
  list(): WindowMeta[] { return [...this.wins.values()] }
  count(): number { return this.wins.size }
  isLast(): boolean { return this.wins.size <= 1 }
}
