import { CLIENT_COMMAND_READ_WATCH, MAX_CLIENT_FAMILY_CONTROL_WAITS, MAX_CLIENT_CONTROLS, MAX_CLIENT_URGENT_CONTROLS, MAX_COMMAND_BODY_BYTES, MAX_URGENT_CONTROL_RETAINED_BYTES } from "../../../../protocol/types"
import { ClientAllocationOwner, type ClientAllocationLease } from "../client-allocation"
import { jsonEncodedBytes } from "../json-size"
import { CLIENT_COMMAND_EXECUTION, CLIENT_COMMAND_LANE, MAX_CLIENT_READS, type ClientCommand, type CommandReply } from "../protocol"
import type { ClientDiagnostics } from "../client-diagnostics"
import { retainedJsonBytes } from "../retained-json"
import { EngineTransportError } from "./errors"

export const MAX_RETAINED_CLIENT_READS = 32
export const MAX_CLIENT_READ_REQUEST_BYTES = 1024 * 1024

// Includes queue bookkeeping in addition to separately measured graphs and serialization.
const ENTRY_BYTES = 768

type Execute = (command: ClientCommand, signal: AbortSignal | undefined, prepare: (authenticated: ClientCommand) => void) => Promise<CommandReply>
interface Entry {
  readonly command: ClientCommand
  readonly signal: AbortSignal | undefined
  readonly execute: Execute
  readonly allocation: ClientAllocationLease
  readonly retained: number
  bytes: number
  readonly enqueuedAt: number | undefined
  readonly resolve: (reply: CommandReply) => void
  readonly reject: (error: unknown) => void
  readonly abort: () => void
  phase: "queued" | "active" | "settled"
}

/** Shared immutable request ownership; reads queue, while control lanes admit immediately. */
export class ClientCommandAdmission {
  readonly #diagnostics: ClientDiagnostics | undefined
  readonly #waiting: Entry[] = []
  #active = 0
  #bytes = 0
  #normal = 0
  #urgent = 0
  #watches = 0

  constructor(diagnostics?: ClientDiagnostics, readonly allocations = new ClientAllocationOwner()) { this.#diagnostics = diagnostics }
  get watchUsage(): number { return this.#watches }
  get controlUsage() { return { normal: this.#normal, urgent: this.#urgent } }

  get usage(): { readonly active: number; readonly queued: number; readonly bytes: number } {
    return { active: this.#active, queued: this.#waiting.length, bytes: this.#bytes }
  }

  async run(command: ClientCommand, signal: AbortSignal | undefined, execute: Execute): Promise<CommandReply> {
    signal?.throwIfAborted()
    if (CLIENT_COMMAND_READ_WATCH[command.type]) return this.#watch(command, signal, execute)
    if (CLIENT_COMMAND_EXECUTION[command.type] !== "read") return this.#control(command, signal, execute)
    if (this.#active + this.#waiting.length >= MAX_RETAINED_CLIENT_READS) {
      throw new EngineTransportError("client read queue count exhausted")
    }
    const remaining = MAX_CLIENT_READ_REQUEST_BYTES - this.#bytes
    const retained = retainedJsonBytes(command, remaining)
    const bytes = requestBytes(command, retained, remaining, MAX_COMMAND_BODY_BYTES)
    const allocation = this.allocations.reserve("outbound", bytes)
    let snapshot: ClientCommand
    try { snapshot = structuredClone(command) } catch (error) { allocation.release(); throw error }
    this.#bytes += bytes
    return new Promise<CommandReply>((resolve, reject) => {
      const entry: Entry = {
        command: snapshot, signal, execute, allocation, retained, bytes, resolve, reject, phase: "queued", enqueuedAt: this.#diagnostics?.start(),
        abort: () => {
          if (entry.phase !== "queued") return
          this.#waiting.splice(this.#waiting.indexOf(entry), 1)
          this.#release(entry)
          reject(signal?.reason ?? new DOMException("read cancelled", "AbortError"))
        },
      }
      this.#waiting.push(entry)
      signal?.addEventListener("abort", entry.abort, { once: true })
      if (signal?.aborted) entry.abort()
      this.#drain()
    })
  }

  async #watch(command: ClientCommand, signal: AbortSignal | undefined, execute: Execute): Promise<CommandReply> {
    if (this.#watches >= MAX_CLIENT_FAMILY_CONTROL_WAITS) throw new EngineTransportError("client read watch count exhausted")
    const retained = retainedJsonBytes(command, MAX_CLIENT_READ_REQUEST_BYTES)
    const bytes = requestBytes(command, retained, MAX_CLIENT_READ_REQUEST_BYTES, MAX_COMMAND_BODY_BYTES)
    using allocation = this.allocations.reserve("outbound", bytes)
    const snapshot = structuredClone(command)
    this.#watches++
    try {
      signal?.throwIfAborted()
      return await execute(snapshot, signal, authenticated => allocation.resize(requestBytes(authenticated, retained, MAX_CLIENT_READ_REQUEST_BYTES, MAX_COMMAND_BODY_BYTES)))
    } finally { this.#watches-- }
  }

  async #control(command: ClientCommand, signal: AbortSignal | undefined, execute: Execute): Promise<CommandReply> {
    const urgent = CLIENT_COMMAND_LANE[command.type] === "urgent"
    if ((urgent ? this.#urgent : this.#normal) >= (urgent ? MAX_CLIENT_URGENT_CONTROLS : MAX_CLIENT_CONTROLS)) {
      throw new EngineTransportError("client control count exhausted")
    }
    const maximum = urgent ? MAX_URGENT_CONTROL_RETAINED_BYTES * 32 : this.allocations.limits.outbound
    const bodyLimit = urgent ? Math.min(MAX_COMMAND_BODY_BYTES, MAX_URGENT_CONTROL_RETAINED_BYTES * 8) : MAX_COMMAND_BODY_BYTES
    const retained = retainedJsonBytes(command, maximum)
    const bytes = requestBytes(command, retained, maximum, bodyLimit)
    using allocation = this.allocations.reserve(urgent ? "urgent" : "outbound", bytes)
    const snapshot = structuredClone(command)
    if (urgent) this.#urgent++; else this.#normal++
    try {
      signal?.throwIfAborted()
      return await execute(snapshot, signal, authenticated => allocation.resize(requestBytes(authenticated, retained, maximum, bodyLimit)))
    } finally { if (urgent) this.#urgent--; else this.#normal-- }
  }

  #drain(): void {
    while (this.#active < MAX_CLIENT_READS) {
      const entry = this.#waiting.shift()
      if (entry === undefined) return
      if (entry.enqueuedAt !== undefined) this.#diagnostics?.finish("read_queue_age", entry.enqueuedAt)
      entry.phase = "active"
      entry.signal?.removeEventListener("abort", entry.abort)
      this.#active += 1
      void this.#execute(entry)
    }
  }

  async #execute(entry: Entry): Promise<void> {
    try {
      entry.signal?.throwIfAborted()
      entry.resolve(await entry.execute(entry.command, entry.signal, authenticated => {
        const bytes = requestBytes(authenticated, entry.retained, MAX_CLIENT_READ_REQUEST_BYTES - this.#bytes + entry.bytes, MAX_COMMAND_BODY_BYTES)
        entry.allocation.resize(bytes)
        this.#bytes += bytes - entry.bytes; entry.bytes = bytes
      }))
    } catch (error) {
      entry.reject(error)
    } finally {
      // Execute includes consuming and validating the HTTP reply body. Cancelling
      // a running request never releases a slot before that operation settles.
      this.#active -= 1
      this.#release(entry)
      this.#drain()
    }
  }

  #release(entry: Entry): void {
    entry.phase = "settled"
    entry.signal?.removeEventListener("abort", entry.abort)
    this.#bytes -= entry.bytes
    entry.allocation.release()
  }
}

/** Capture and authenticated envelope graphs plus encoded string and UTF-8 fetch body. */
function requestBytes(command: ClientCommand, captured: number, maximum: number, bodyLimit: number): number {
  const retained = retainedJsonBytes(command, maximum)
  const encoded = jsonEncodedBytes(command, bodyLimit)
  const bytes = captured + retained + 3 * encoded + ENTRY_BYTES
  if (encoded > bodyLimit || bytes > maximum) throw new EngineTransportError("client request byte allowance exhausted")
  return bytes
}
