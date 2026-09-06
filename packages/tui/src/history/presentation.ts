import { ClientCache } from "./cache"
import type { ClientAllocationOwner } from "../client-allocation"
import type { ClientDiagnostics } from "../client-diagnostics"
import { HistoryController, type HistorySnapshot } from "./controller"
import type { SessionReader, SessionReadTarget } from "../session-reader"

/** Coalesce source invalidations without cancelling a page read that is making progress. */
export class HistoryPresentation {
  readonly controller: HistoryController
  readonly #changed: (snapshot: HistorySnapshot) => void
  readonly #diagnostics: ClientDiagnostics | undefined
  #queuedAt: number | undefined
  #dirty = false
  #timer: ReturnType<typeof setTimeout> | null = null
  #disposed = false
  #nextReadAt = 0

  constructor(reader: Pick<SessionReader, "page" | "content">, changed: (snapshot: HistorySnapshot) => void, diagnostics?: ClientDiagnostics, allocations?: ClientAllocationOwner) {
    this.#diagnostics = diagnostics
    this.#changed = changed
    this.controller = new HistoryController(reader, () => {
      this.#changed(this.controller.snapshot)
      this.#schedule()
    }, new ClientCache(undefined, allocations), diagnostics)
  }

  present(target: SessionReadTarget): void {
    if (this.#disposed || this.controller.target === target) return
    this.#dirty = false
    this.#queuedAt = undefined
    this.#clearTimer()
    this.#nextReadAt = performance.now() + 100
    void this.controller.open(target)
  }

  invalidate(sessionId: string): void {
    if (this.#disposed || this.controller.snapshot.sessionId !== sessionId) return
    this.#queuedAt ??= this.#diagnostics?.start()
    this.#dirty = true
    this.#schedule()
  }

  suspend(): void {
    this.#dirty = false; this.#queuedAt = undefined; this.#clearTimer()
    this.controller.suspend()
  }

  dispose(): void {
    this.#disposed = true
    this.#clearTimer()
    this.controller.dispose()
  }

  #schedule(): void {
    if (this.#disposed || !this.#dirty || this.#timer !== null || this.controller.snapshot.loading) return
    this.#timer = setTimeout(() => {
      this.#timer = null
      if (this.#queuedAt !== undefined) this.#diagnostics?.finish("history_queue_age", this.#queuedAt)
      this.#queuedAt = undefined
      this.#dirty = false
      this.#nextReadAt = performance.now() + 100
      void this.controller.refresh()
    }, Math.max(0, this.#nextReadAt - performance.now()))
  }

  #clearTimer(): void {
    if (this.#timer !== null) clearTimeout(this.#timer)
    this.#timer = null
  }
}
