import { setTimeout as delay } from "node:timers/promises"
import { MAX_FAMILY_CONTROLS_PREPARED_BYTES, MAX_SESSION_STATE_PREPARED_BYTES, type ChildControlTarget, type SessionStateSnapshot, type TranscriptTailPage } from "../../../protocol/types"
import type { ClientAllocationLease } from "./client-allocation"
import { ClientAllocationError } from "./client-allocation"
import type { ClientCache } from "./history/cache"
import type { HistoryCacheValue } from "./history/controller"
import { collectLiveTail, TailChanged, type TailRead } from "./history/live-tail"
import { retainedJsonBytes } from "./retained-json"
import type { SessionReadTarget } from "./session-reader"
import type { ReplyAllocation } from "./transport/reply-allocation"

const REFRESH_MILLIS = 250
interface Selection {
  readonly root: string
  readonly target: ChildControlTarget
  readonly source: SessionReadTarget
  readonly abort: AbortController
  readonly allocation: ClientAllocationLease
  references: number
}
function release(selection: Selection): void { if (--selection.references === 0) selection.allocation.release() }
interface Options {
  readonly cache: ClientCache<HistoryCacheValue>
  readState(root: string, target: ChildControlTarget, signal: AbortSignal, allocation: ReplyAllocation): Promise<SessionStateSnapshot>
  readonly readTail: TailRead
  apply(snapshot: SessionStateSnapshot, pages: readonly TranscriptTailPage[] | null): void
  failed(message: string | null): void
}

/** One selected actor and one settling read worker; progress delivery is not a bootstrap dependency. */
export class ChildDisplayController {
  #selection: Selection | null = null
  #running = false
  #work: Promise<void> = Promise.resolve()
  constructor(readonly options: Options) {}
  open(root: string, target: ChildControlTarget, source: SessionReadTarget): void {
    const bytes = retainedJsonBytes({ root, target, source }, MAX_FAMILY_CONTROLS_PREPARED_BYTES)
    if (bytes > MAX_FAMILY_CONTROLS_PREPARED_BYTES) throw new ClientAllocationError("selected child display binding is too large")
    const allocation = this.options.cache.allocations.reserve("children", bytes)
    let next: Selection
    try { next = { root, target: structuredClone(target), source: structuredClone(source), allocation, abort: new AbortController(), references: 1 } }
    catch (error) { allocation.release(); throw error }
    this.close()
    this.#selection = next
    this.#start()
  }
  close(): void {
    const previous = this.#selection; this.#selection = null
    previous?.abort.abort()
    if (previous !== null) release(previous)
  }
  async settled(): Promise<void> { await this.#work }
  #start(): void {
    if (this.#running) return
    this.#running = true
    this.#work = this.#run().finally(() => { this.#running = false; if (this.#selection !== null) this.#start() })
  }
  async #run(): Promise<void> {
    while (this.#selection !== null) {
      const selected = this.#selection
      selected.references++
      try { await this.#poll(selected) }
      finally { release(selected) }
    }
  }
  #notify(message: string | null): void {
    try { this.options.failed(message) }
    catch {
      // A renderer already refusing replacement cannot safely receive another error render.
      // The application stores the error before refreshing; stop this worker until the view reopens.
      this.close()
    }
  }
  async #poll(selected: Selection): Promise<void> {
    let through: string | null | undefined, compaction: string | null = null
    while (!selected.abort.signal.aborted) {
      let tail: Awaited<ReturnType<typeof collectLiveTail>> | null = null
      try {
        using allocation = this.options.cache.allocations.reserve("metadata", 0)
        const snapshot = await this.options.readState(selected.root, selected.target, selected.abort.signal, { admit(bytes) {
          if (bytes > MAX_SESSION_STATE_PREPARED_BYTES) throw new ClientAllocationError("child metadata exceeds its prepared allowance")
          allocation.admit(bytes)
        } })
        selected.abort.signal.throwIfAborted()
        const bytes = retainedJsonBytes(snapshot, MAX_SESSION_STATE_PREPARED_BYTES)
        if (bytes > MAX_SESSION_STATE_PREPARED_BYTES) throw new ClientAllocationError("child metadata exceeds its prepared allowance")
        allocation.resize(Math.max(allocation.bytes, bytes))
        if (through !== undefined && through !== null && (snapshot.through === null || BigInt(snapshot.through) < BigInt(through))) throw new TailChanged("child metadata prefix moved backwards")
        const revision = snapshot.compaction === null ? null : `${snapshot.compaction.started}:${snapshot.compaction.revision}`
        if (snapshot.through !== through) {
          tail = await collectLiveTail(this.options.readTail, this.options.cache, selected.source, selected.abort.signal)
          if (tail.pages[0]?.identity.turn_started !== (snapshot.active_turn?.started ?? null)) throw new TailChanged("child active turn changed during display recovery")
          if (snapshot.through !== null && tail.pages.some(page => page.view.through === null || BigInt(page.view.through) < BigInt(snapshot.through!))) throw new TailChanged("child display projection predates its metadata snapshot")
        }
        selected.abort.signal.throwIfAborted()
        if (this.#selection !== selected) return
        if (tail !== null || revision !== compaction) this.options.apply(snapshot, tail?.pages ?? null)
        through = snapshot.through; compaction = revision
        if (this.#selection !== selected) return
        this.#notify(null)
      } catch (error) {
        if (selected.abort.signal.aborted) return
        this.#notify(error instanceof Error ? error.message.slice(0, 512) : "Child display could not be recovered.")
      } finally { tail?.release() }
      await delay(REFRESH_MILLIS, undefined, { signal: selected.abort.signal }).catch(() => {})
    }
  }
}
