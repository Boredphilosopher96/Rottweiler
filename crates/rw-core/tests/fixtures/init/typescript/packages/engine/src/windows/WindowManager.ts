import { WindowRegistry, type WindowMeta } from './WindowRegistry'

/** A window the manager controls. In production it wraps a BaseWindow + Browser;
 *  in tests it's a fake recording focus/close. */
export interface ManagedWindow {
  id: string
  focus(): void
  close(): void
}

export type WindowFactory = (id: string) => ManagedWindow

/**
 * Coordinates multiple browser windows over the pure WindowRegistry. Owns window
 * creation/teardown and focus, and keeps the registry (focus order, tab counts)
 * in sync. The Electron factory supplies real windows; the IPC fan-out that
 * routes renderer messages to the focused window's Browser is the remaining wire.
 */
export class WindowManager {
  private registry = new WindowRegistry()
  private windows = new Map<string, ManagedWindow>()
  private seq = 0

  constructor(private readonly factory: WindowFactory) {}

  open(): ManagedWindow {
    const id = `win-${++this.seq}`
    const win = this.factory(id)
    this.windows.set(id, win)
    this.registry.open(id)
    return win
  }

  focus(id: string): void {
    const win = this.windows.get(id)
    if (!win) return
    this.registry.focus(id)
    win.focus()
  }

  /** Close a window we own (calls close() on it). */
  close(id: string): void {
    const win = this.windows.get(id)
    if (!win) return
    win.close()
    this.windows.delete(id)
    this.registry.close(id)
  }

  /** A window closed itself (user hit the X) — just reconcile our bookkeeping. */
  onClosed(id: string): void {
    this.windows.delete(id)
    this.registry.close(id)
  }

  setTabCount(id: string, n: number): void { this.registry.setTabCount(id, n) }

  focused(): ManagedWindow | null {
    const id = this.registry.focused()
    return id ? this.windows.get(id) ?? null : null
  }

  get(id: string): ManagedWindow | null { return this.windows.get(id) ?? null }
  list(): WindowMeta[] { return this.registry.list() }
  count(): number { return this.windows.size }
  isLast(): boolean { return this.windows.size <= 1 }
}
