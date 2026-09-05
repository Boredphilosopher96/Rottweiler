import { CLIENT_COMMAND_EXECUTION, MAX_CLIENT_READS, type ClientCommand, type CommandReply } from "../protocol"
import type { ClientDiagnostics } from "../client-diagnostics"
import { retainedJsonBytes } from "../retained-json"
import { EngineTransportError } from "./errors"

export const MAX_RETAINED_CLIENT_READS = 32
export const MAX_CLIENT_READ_REQUEST_BYTES = 1024 * 1024

// Includes the snapshot, escaped UTF-16 JSON and UTF-8 request buffer. An escaped
// UTF-16 code unit needs at most six JSON characters; the snapshot stays owned.
const SERIALIZATION_CHARGE = 16
const ENTRY_BYTES = 768

type Execute = (command: ClientCommand, signal?: AbortSignal) => Promise<CommandReply>
interface Entry {
  readonly command: ClientCommand
  readonly signal: AbortSignal | undefined
  readonly execute: Execute
  readonly bytes: number
  readonly enqueuedAt: number | undefined
  readonly resolve: (reply: CommandReply) => void
  readonly reject: (error: unknown) => void
  readonly abort: () => void
  phase: "queued" | "active" | "settled"
}

/** One FIFO and byte owner shared by every query feature on a client connection. */
export class ClientReadAdmission {
  readonly #diagnostics: ClientDiagnostics | undefined
  readonly #waiting: Entry[] = []
  #active = 0
  #bytes = 0

  constructor(diagnostics?: ClientDiagnostics) { this.#diagnostics = diagnostics }

  get usage(): { readonly active: number; readonly queued: number; readonly bytes: number } {
    return { active: this.#active, queued: this.#waiting.length, bytes: this.#bytes }
  }

  async run(command: ClientCommand, signal: AbortSignal | undefined, execute: Execute): Promise<CommandReply> {
    signal?.throwIfAborted()
    if (CLIENT_COMMAND_EXECUTION[command.type] !== "read") return execute(command, signal)
    if (this.#active + this.#waiting.length >= MAX_RETAINED_CLIENT_READS) {
      throw new EngineTransportError("client read queue count exhausted")
    }
    const remaining = MAX_CLIENT_READ_REQUEST_BYTES - this.#bytes
    const payloadLimit = Math.max(0, Math.floor((remaining - ENTRY_BYTES) / SERIALIZATION_CHARGE))
    const bytes = retainedJsonBytes(command, payloadLimit) * SERIALIZATION_CHARGE + ENTRY_BYTES
    if (bytes > remaining) throw new EngineTransportError("client read queue byte allowance exhausted")
    // Capture before the first await, so a caller cannot grow a charged request.
    const snapshot = structuredClone(command)
    this.#bytes += bytes
    return new Promise<CommandReply>((resolve, reject) => {
      const entry: Entry = {
        command: snapshot, signal, execute, bytes, resolve, reject, phase: "queued", enqueuedAt: this.#diagnostics?.start(),
        abort: () => {
          if (entry.phase !== "queued") return
          this.#waiting.splice(this.#waiting.indexOf(entry), 1)
          this.#release(entry)
          reject(signal?.reason ?? new DOMException("read cancelled", "AbortError"))
        },
      }
      this.#waiting.push(entry)
      signal?.addEventListener("abort", entry.abort, { once: true })
      this.#drain()
    })
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
      entry.resolve(await entry.execute(entry.command, entry.signal))
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
  }
}
