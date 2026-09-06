import { ToolOutputAllocation } from "./tool-output-allocation"
import { ClientAllocationOwner, type ClientAllocationLease } from "../client-allocation"
import { TOOL_OUTPUT_CACHE_ALLOCATION_BYTES, ToolOutputBuffer, ToolOutputCacheIdentity } from "./display-buffer"
import { createInitialState, type RottweilerState } from "./model"
const EMPTY_DISPOSED_STATE = createInitialState()
const MAX_STAGING_BYTES = 32 * 1024 * 1024
const MAX_STAGING_STEPS = MAX_STAGING_BYTES / 32
interface Parts { readonly bytes: number; readonly children: readonly object[] }
interface Entry { readonly parts: Parts; readonly references: number }

/** The object graph shares unchanged immutable subtrees across model and mounted revisions. */
export class ProjectionGraph {
  readonly #parts = new WeakMap<object, Parts>()
  readonly #streams = new Map<ToolOutputCacheIdentity, ToolOutputAllocation>()
  readonly #entries = new Map<object, Entry>()
  readonly #lease: ClientAllocationLease
  #visited = 0
  constructor(readonly allocations: ClientAllocationOwner) { this.#lease = allocations.reserve("live", 0) }
  get visitedObjects(): number { return this.#visited }
  get retainedObjects(): number { return this.#entries.size }
  get bytes(): number { return this.#lease.bytes }

  /** Stage reference deltas before admission. A refusal cannot mutate the retained graph. */
  replace(added: readonly object[], removed: readonly object[]): void {
    const staging = this.allocations.reserve("live", 0)
    let stagingSteps = 0
    const stage = (bytes: number) => {
      if (++stagingSteps > MAX_STAGING_STEPS || staging.bytes + bytes > MAX_STAGING_BYTES) throw new Error("client projection staging admission exhausted")
      staging.resize(staging.bytes + bytes)
    }
    const newParts = new Map<object, Parts>()
    try {
    const updates = new Map<object, Entry>()
    const streams = new Map<ToolOutputCacheIdentity, ToolOutputAllocation>()
    let bytes = this.#lease.bytes
    const updateStream = (buffer: ToolOutputBuffer, add: boolean) => {
      const identity = buffer.allocationCache
      if (identity === null) return
      let stream = streams.get(identity)
      if (stream === undefined) { stream = new ToolOutputAllocation(this.#streams.get(identity), stage); streams.set(identity, stream) }
      const previous = stream.bytes
      if (add) stream.add(buffer, stage)
      else stream.remove(buffer, stage)
      bytes += stream.bytes - previous
    }
    const apply = (roots: readonly object[], delta: 1 | -1) => {
      const pending = [...roots]
      while (pending.length > 0) {
        const object = pending.pop()!
        const entry = updates.get(object) ?? this.#entries.get(object)
        const before = entry?.references ?? 0, references = before + delta
        if (references < 0) throw new Error("client projection ownership underflow")
        let parts = entry?.parts ?? newParts.get(object) ?? this.#parts.get(object)
        if (parts === undefined) { parts = this.#describe(object, stage); newParts.set(object, parts) }
        if (!updates.has(object)) stage(64)
        updates.set(object, { parts, references })
        if (before === 0 && references === 1) {
          bytes += parts.bytes
          if (object instanceof ToolOutputBuffer) updateStream(object, true)
          if (!this.allocations.canReserve("live", bytes, this.#lease.bytes)) throw new Error("client projection allocation admission exhausted")
          for (const child of parts.children) pending.push(child)
        } else if (before === 1 && references === 0) {
          bytes -= parts.bytes
          if (object instanceof ToolOutputBuffer) updateStream(object, false)
          for (const child of parts.children) pending.push(child)
        }
      }
    }
    apply(added, 1); apply(removed, -1)
    this.#lease.resize(bytes)
    for (const [object, parts] of newParts) this.#parts.set(object, parts)
    for (const [identity, stream] of streams) {
      if (stream.empty) this.#streams.delete(identity)
      else this.#streams.set(identity, stream)
    }
    for (const [object, entry] of updates) {
      if (entry.references === 0) this.#entries.delete(object)
      else this.#entries.set(object, entry)
    }
    } finally { staging.release() }
  }
  #describe(value: object, stage: (bytes: number) => void): Parts {
    stage(128)
    if (value instanceof ToolOutputCacheIdentity) return { bytes: TOOL_OUTPUT_CACHE_ALLOCATION_BYTES + 192, children: [] }
    this.#visited++
    let bytes = 48
    const children: object[] = []
    if (value instanceof ToolOutputBuffer) {
      bytes = 128
      if (value.allocationCache !== null) children.push(value.allocationCache)
    } else {
      const array = Array.isArray(value)
      if (array) bytes = 32 + value.length * 8
      for (const key in value) {
        stage(0)
        if (!Object.hasOwn(value, key)) continue
        if (!array) bytes += 48 + key.length * 2
        const child: unknown = Reflect.get(value, key)
        if (typeof child === "string") bytes += 24 + child.length * 2
        else if (child === null || typeof child === "number" || typeof child === "boolean") bytes += 8
        else if (typeof child === "object") { stage(8); children.push(child) }
        else throw new Error("client projection contains non-data state")
      }
    }
    const parts = { bytes: bytes + 192, children }
    return parts
  }
  dispose(): void { this.#streams.clear(); this.#entries.clear(); this.#lease.release() }
}

type ProjectionSlot = "root" | "child" | "mountedRoot" | "mountedChild"

/** Each slot keeps its reference until the owning component completes replacement. */
export class ProjectionAllocations {
  readonly graph: ProjectionGraph
  readonly #slots = new Map<ProjectionSlot, RottweilerState>()
  #disposed = false
  constructor(readonly allocations: ClientAllocationOwner) {
    this.graph = new ProjectionGraph(allocations)
    this.set("root", createInitialState())
  }
  get root(): RottweilerState { return this.#slots.get("root") ?? EMPTY_DISPOSED_STATE }
  get child(): RottweilerState | null { return this.#slots.get("child") ?? null }
  set(slot: "root" | "child", state: RottweilerState | null): void {
    if (this.#disposed) throw new Error("client projections are disposed")
    const previous = this.#slots.get(slot)
    if (previous === state) return
    this.graph.replace(state === null ? [] : [state], previous === undefined ? [] : [previous])
    if (state === null) this.#slots.delete(slot)
    else this.#slots.set(slot, state)
  }
  retain(): Pick<ClientAllocationLease, "release"> {
    const held = [this.root, ...(this.child === null ? [] : [this.child])]
    this.graph.replace(held, [])
    let active = true
    return { release: () => { if (!active || this.#disposed) return; active = false; this.graph.replace([], held) } }
  }
  presented(): void {
    const prior = [this.#slots.get("mountedRoot"), this.#slots.get("mountedChild")].filter((state): state is RottweilerState => state !== undefined)
    const current = [this.root, ...(this.child === null ? [] : [this.child])]
    this.graph.replace(current, prior)
    this.#slots.set("mountedRoot", this.root)
    if (this.child === null) this.#slots.delete("mountedChild")
    else this.#slots.set("mountedChild", this.child)
  }
  dispose(): void { this.#disposed = true; this.#slots.clear(); this.graph.dispose() }
}
