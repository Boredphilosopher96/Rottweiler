import { MAX_ACTIVE_CHILDREN, MAX_CHILD_TASK_PREVIEW_BYTES } from "../../../protocol/types"
import { ClientAllocationError, type ClientAllocationLease, type ClientAllocationOwner } from "./client-allocation"
import { retainedJsonBytes } from "./retained-json"
import { sanitizeSubagentDescriptor, type SubagentDescriptor } from "./subagent-state"

// Bounded strings, their normalized copies, row/array objects and temporary preview work.
const ROW_PREPARATION_BYTES = MAX_CHILD_TASK_PREVIEW_BYTES * 16 + 4096

/** The retained catalog is independent from decoded replies and mounted picker revisions. */
export class SubagentCatalog {
  #values: readonly SubagentDescriptor[] = []
  #allocation: ClientAllocationLease | null = null
  constructor(readonly owner: ClientAllocationOwner) {}
  get values(): readonly SubagentDescriptor[] { return this.#values }
  replace(values: readonly SubagentDescriptor[]): void {
    if (values.length > MAX_ACTIVE_CHILDREN) throw new ClientAllocationError("child catalog exceeds its admitted actor count")
    this.#replace(values.length, () => values.map(sanitizeSubagentDescriptor).filter((value): value is SubagentDescriptor => value !== null))
  }
  activity(id: string, activity: SubagentDescriptor["activity"]): void {
    if (!this.#values.some(value => value.subagent_id === id && value.activity !== activity)) return
    this.#replace(this.#values.length, () => this.#values.map(value => value.subagent_id === id ? { ...value, activity } : value))
  }
  remove(id: string): void {
    if (!this.#values.some(value => value.subagent_id === id)) return
    this.#replace(this.#values.length, () => this.#values.filter(value => value.subagent_id !== id))
  }
  clear(): void {
    this.#values = []
    this.#allocation?.release(); this.#allocation = null
  }
  #replace(count: number, prepare: () => readonly SubagentDescriptor[]): void {
    const incoming = this.owner.reserve("children", count * ROW_PREPARATION_BYTES + 256)
    try {
      const values = prepare()
      const bytes = retainedJsonBytes(values, incoming.bytes)
      if (bytes > incoming.bytes) throw new ClientAllocationError("child catalog preparation exceeded its reservation")
      incoming.resize(bytes)
      const prior = this.#allocation
      this.#values = values; this.#allocation = incoming
      prior?.release()
    } catch (error) { incoming.release(); throw error }
  }
}
