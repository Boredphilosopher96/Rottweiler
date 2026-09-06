import type { ChildControlResponse, ChildControlTarget, FamilyControlRow, FamilyControlsSnapshot, SessionControlsSnapshot } from "../../../protocol/types"
import { MAX_FAMILY_CONTROLS_PREPARED_BYTES, MAX_SESSION_CONTROLS_PREPARED_BYTES } from "../../../protocol/types"
import { type ClientAllocationLease, ClientAllocationOwner } from "./client-allocation"
import { sameChildTarget, type FamilyControlsReader } from "./family-controls-reader"
import { retainedJsonBytes } from "./retained-json"
import { setTimeout as delay } from "node:timers/promises"

interface Selection { root: string | null; target: ChildControlTarget; allocation: ClientAllocationLease; abort: AbortController; references: number }
function releaseSelection(value: Selection): void { if (--value.references === 0) value.allocation.release() }

interface FamilyControlsOptions {
  readonly allocations: ClientAllocationOwner
  readonly reader: FamilyControlsReader
  changed(): void
  apply(snapshot: SessionControlsSnapshot | null): void
}

/** One watch and one selected snapshot; reconnect establishes a new live revision namespace. */
export class FamilyControlsController {
  #connection: AbortController | null = null
  #root: string | null = null
  #watchWork: Promise<void> = Promise.resolve()
  #watchRunning = false
  #selectionWork: Promise<void> = Promise.resolve()
  #selection: Selection | null = null
  #selectionDirty = false
  #selectionRunning = false
  #responses = 0
  #revision: string | null = null
  #snapshot: FamilyControlsSnapshot | null = null
  #snapshotAllocation: ClientAllocationLease | null = null
  #error: string | null = null
  constructor(readonly options: FamilyControlsOptions) {}
  get rows(): readonly FamilyControlRow[] { return this.#snapshot?.children ?? [] }
  get error(): string | null { return this.#error }
  get target(): ChildControlTarget | null { return this.#selection?.target ?? null }
  get pendingResponses(): boolean { return this.#responses !== 0 }
  get ready(): boolean { return this.#revision !== null }
  get pending(): readonly FamilyControlRow[] { return this.rows.filter(row => row.controls.questions + row.controls.approvals > 0 || row.controls.pending_plan) }

  connect(root: string | null): void {
    if (root === this.#root) return
    this.#connection?.abort()
    this.#connection = root === null ? null : new AbortController()
    this.#root = root
    if (root !== null && this.#selection !== null && this.#selection.root !== root) this.select(null)
    this.#revision = null
    this.#error = null
    this.#clearSnapshot()
    this.#selection?.abort.abort()
    if (this.#selection !== null) this.#selection.abort = new AbortController()
    if (root === null) return
    this.#startWatch()
  }

  refresh(): void { const root = this.#root; this.connect(null); this.connect(root) }

  select(target: ChildControlTarget | null): void {
    if (target !== null && this.#selection !== null && sameChildTarget(target, this.#selection.target)) { this.retry(); return }
    let next: Selection | null = null
    if (target !== null) {
      const allocation = this.options.allocations.reserve("children", retainedJsonBytes(target, MAX_FAMILY_CONTROLS_PREPARED_BYTES))
      try { next = { root: this.#root, target: structuredClone(target), allocation, abort: new AbortController(), references: 1 } }
      catch (error) { allocation.release(); throw error }
    }
    const prior = this.#selection
    this.#selection = next
    prior?.abort.abort()
    this.#revision = null
    this.#selectionDirty = false
    try { this.options.apply(null); this.options.changed() } finally { if (prior !== null) releaseSelection(prior) }
    this.retry()
  }

  retry(): void {
    this.#selectionDirty = true
    if (this.#selectionRunning) return
    this.#selectionRunning = true
    this.#selectionWork = this.#selectionWork.then(() => this.#readSelected()).catch(error => this.#fail(error)).finally(() => {
      this.#selectionRunning = false
      if (this.#selectionDirty && this.#root !== null && this.#selection !== null) this.retry()
    })
  }

  async respond<T>(response: ChildControlResponse, execute: (root: string, target: ChildControlTarget, revision: string, response: ChildControlResponse) => Promise<T>): Promise<T> {
    const root = this.#root, selection = this.#selection, revision = this.#revision
    if (root === null || selection === null || revision === null) throw new Error("Child controls are refreshing; wait for the authoritative snapshot.")
    selection.references++; this.#responses++
    try { return await execute(root, selection.target, revision, response) }
    finally { this.#responses--; releaseSelection(selection); if (this.#selection === selection && this.#root === root) { this.#revision = null; this.retry() } }
  }

  close(): void {
    this.connect(null)
    this.select(null)
  }
  async settled(): Promise<void> { await Promise.all([this.#watchWork, this.#selectionWork]) }

  #startWatch(): void {
    if (this.#watchRunning) return
    this.#watchRunning = true
    this.#watchWork = (async () => {
      while (this.#root !== null && this.#connection !== null) {
        const root = this.#root, connection = this.#connection
        try { await this.#watch(root, connection) }
        catch (error) { if (!connection.signal.aborted) { this.#fail(error); return } }
        if (this.#connection === connection) return
      }
    })().finally(() => { this.#watchRunning = false })
  }

  async #watch(root: string, connection: AbortController): Promise<void> {
    let after: string | null = null
    while (!connection.signal.aborted) {
      let allocation: ClientAllocationLease | null = this.options.allocations.reserve("children", 0)
      try {
        const snapshot = await this.options.reader.watch(root, after, connection.signal, bounded(allocation, MAX_FAMILY_CONTROLS_PREPARED_BYTES))
        connection.signal.throwIfAborted()
        if (after !== null && BigInt(snapshot.revision) < BigInt(after)) throw new Error("Family control reply predates the observed live revision.")
        allocation.resize(retainedJsonBytes(snapshot, MAX_FAMILY_CONTROLS_PREPARED_BYTES))
        const prior = this.#snapshotAllocation
        this.#snapshot = snapshot; this.#snapshotAllocation = allocation; allocation = null
        this.#error = null; after = snapshot.revision
        const selected = this.#selection
        const row = selected === null ? undefined : snapshot.children.find(item => sameChildTarget(item.target, selected.target))
        try {
          if (selected !== null && (!row?.controls.available || (this.#revision !== null && BigInt(row.controls.revision) > BigInt(this.#revision)))) this.#revision = null
          this.options.changed()
          if (selected !== null && this.#revision === null) this.retry()
        } finally { prior?.release() }
      } catch (error) {
        if (connection.signal.aborted) return
        this.#fail(error)
        await delay(1000, undefined, { signal: connection.signal }).catch(() => {})
      } finally { allocation?.release() }
    }
  }

  async #readSelected(): Promise<void> {
    while (this.#selectionDirty) {
      this.#selectionDirty = false
      const selection = this.#selection, connection = this.#connection, root = this.#root
      if (selection === null || connection === null || root === null) return
      if (!this.rows.some(item => sameChildTarget(item.target, selection.target) && item.controls.available)) { this.#revision = null; this.options.apply(null); this.options.changed(); return }
      using allocation = this.options.allocations.reserve("controls", 0)
      const signal = AbortSignal.any([selection.abort.signal, connection.signal])
      selection.references++
      try {
        const result = await this.options.reader.child(root, selection.target, signal, bounded(allocation, MAX_SESSION_CONTROLS_PREPARED_BYTES))
        signal.throwIfAborted()
        if (selection !== this.#selection || root !== this.#root) continue
        const current = this.rows.find(item => sameChildTarget(item.target, selection.target))
        if (!current?.controls.available) continue
        if (BigInt(result.revision) < BigInt(current.controls.revision)) { this.#selectionDirty = true; continue }
        this.options.apply(result.snapshot)
        this.#revision = result.revision
        this.#error = null
        this.options.changed()
      } catch (error) { if (!signal.aborted) this.#fail(error) }
      finally { releaseSelection(selection) }
    }
  }

  #clearSnapshot(): void {
    this.#snapshot = null
    const prior = this.#snapshotAllocation; this.#snapshotAllocation = null
    try { this.options.changed() } finally { prior?.release() }
  }
  #fail(error: unknown): void { this.#error = error instanceof Error ? error.message.slice(0, 512) : "Child controls could not be read."; this.options.changed() }
}

function bounded(allocation: ClientAllocationLease, maximum: number): Pick<ClientAllocationLease, "admit"> {
  return { admit(bytes) { if (bytes > maximum) throw new Error("Child control snapshot exceeds its prepared allowance."); allocation.admit(bytes) } }
}
