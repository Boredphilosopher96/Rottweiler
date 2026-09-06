import type { EngineEvent } from "./protocol"
import type { ClientDiagnostics } from "./client-diagnostics"
export interface PresentationFrameScheduler {
  schedule(callback: () => void, delayMs: number): unknown
  cancel(handle: unknown): void
}

interface PresentationControllerOptions<T> {
  readonly diagnostics?: ClientDiagnostics | undefined
  readonly scheduler: PresentationFrameScheduler | undefined
  readonly destroyed: () => boolean
  readonly present: (pending: readonly T[], dirty: boolean) => void
  readonly afterPresent: (item: T) => void
}

export class PresentationController<T> {
  readonly #options: PresentationControllerOptions<T>
  #queue: T[] = []
  #queuedAt: number | undefined
  #dirty = false
  #frameHandle: unknown | null = null
  #presenting = false
  #suspended = false
  #pendingDisplay = false
  #lastFlushAt = performance.now() - 16

  constructor(options: PresentationControllerOptions<T>) {
    this.#options = options
  }

  enqueue(item: T, deferToFrame: boolean): void {
    this.#queuedAt ??= this.#options.diagnostics?.start()
    if (this.#suspended) {
      this.#queue = [item]
      this.#pendingDisplay = deferToFrame
      return
    }
    if (deferToFrame && this.#pendingDisplay && this.#queue.length > 0) this.#queue[this.#queue.length - 1] = item
    else this.#queue.push(item)
    this.#pendingDisplay = deferToFrame
    if (this.#suspended) return
    if (deferToFrame) this.#scheduleFrame()
    else this.flush()
  }

  markDirty(deferToFrame: boolean): void {
    this.#queuedAt ??= this.#options.diagnostics?.start()
    this.#dirty = true
    if (this.#suspended) return
    if (deferToFrame) this.#scheduleFrame()
    else this.flush()
  }

  flushBeforeStateChange(): void {
    if (!this.#suspended && !this.#presenting && this.#queue.length > 0) this.flush()
  }

  flush(): void {
    if (this.#suspended) return
    this.#cancelFrame()
    if (this.#options.destroyed() || (this.#queue.length === 0 && !this.#dirty)) return
    const startedAt = this.#options.diagnostics?.start()
    if (startedAt !== undefined && this.#queuedAt !== undefined) {
      this.#options.diagnostics?.record("presentation_queue_age", startedAt - this.#queuedAt)
    }
    this.#queuedAt = undefined
    const pending = this.#queue
    this.#queue = []
    this.#pendingDisplay = false
    const dirty = this.#dirty
    this.#dirty = false
    this.#presenting = true
    try {
      this.#options.present(pending, dirty)
    } finally {
      this.#presenting = false
      if (startedAt !== undefined) this.#options.diagnostics?.finish("presentation", startedAt, pending.length)
    }
    this.#lastFlushAt = performance.now()
    for (const item of pending) this.#options.afterPresent(item)
  }

  destroy(): void {
    this.#queuedAt = undefined
    this.#cancelFrame()
    this.#queue = []
    this.#pendingDisplay = false
    this.#dirty = false
  }

  suspend(): void {
    this.#suspended = true
    if (this.#queue.length > 1) this.#queue = [this.#queue.at(-1)!]
    this.#cancelFrame()
  }

  resume(): void {
    if (!this.#suspended) return
    this.#suspended = false
    this.flush()
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

/** Only state-only updates can replace a pending frame; all control/read effects present immediately. */
const DISPLAY_ONLY_EVENTS = new Set<EngineEvent["type"]>([
  "text_delta", "thinking_delta", "citation_delta", "tool_output_delta", "tool_progress",
  "compaction_text_delta", "compaction_thinking_delta", "subagent_progress", "context_usage_updated",
])

export function deferPresentationForEvent(event: { readonly type: EngineEvent["type"] }): boolean {
  return DISPLAY_ONLY_EVENTS.has(event.type)
}
