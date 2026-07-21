export interface PresentationFrameScheduler {
  schedule(callback: () => void, delayMs: number): unknown
  cancel(handle: unknown): void
}

interface PresentationControllerOptions<T> {
  readonly scheduler: PresentationFrameScheduler | undefined
  readonly destroyed: () => boolean
  readonly present: (pending: readonly T[], dirty: boolean) => void
  readonly afterPresent: (item: T) => void
}

export class PresentationController<T> {
  readonly #options: PresentationControllerOptions<T>
  #queue: T[] = []
  #dirty = false
  #frameHandle: unknown | null = null
  #presenting = false
  #lastFlushAt = performance.now() - 16

  constructor(options: PresentationControllerOptions<T>) {
    this.#options = options
  }

  enqueue(item: T, deferToFrame: boolean): void {
    this.#queue.push(item)
    if (deferToFrame) this.#scheduleFrame()
    else this.flush()
  }

  markDirty(deferToFrame: boolean): void {
    this.#dirty = true
    if (deferToFrame) this.#scheduleFrame()
    else this.flush()
  }

  flushBeforeStateChange(): void {
    if (!this.#presenting && this.#queue.length > 0) this.flush()
  }

  flush(): void {
    this.#cancelFrame()
    if (this.#options.destroyed() || (this.#queue.length === 0 && !this.#dirty)) return
    const pending = this.#queue
    this.#queue = []
    const dirty = this.#dirty
    this.#dirty = false
    this.#presenting = true
    try {
      this.#options.present(pending, dirty)
    } finally {
      this.#presenting = false
    }
    this.#lastFlushAt = performance.now()
    for (const item of pending) this.#options.afterPresent(item)
  }

  destroy(): void {
    this.#cancelFrame()
    this.#queue = []
    this.#dirty = false
  }

  #scheduleFrame(): void {
    if (this.#frameHandle !== null) return
    const scheduler = this.#options.scheduler
    if (scheduler === undefined) {
      const elapsed = performance.now() - this.#lastFlushAt
      // Show the first token after an idle frame immediately, then coalesce
      // deltas that arrive inside the active 16 ms presentation window.
      if (elapsed >= 16) {
        this.flush()
        return
      }
      this.#frameHandle = setTimeout(() => this.flush(), Math.max(0, 16 - elapsed))
      return
    }
    this.#frameHandle = scheduler.schedule(() => this.flush(), 16)
  }

  #cancelFrame(): void {
    const handle = this.#frameHandle
    if (handle === null) return
    this.#frameHandle = null
    const scheduler = this.#options.scheduler
    if (scheduler === undefined) clearTimeout(handle as ReturnType<typeof setTimeout>)
    else scheduler.cancel(handle)
  }
}
