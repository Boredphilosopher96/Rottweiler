import { boundedJsonStringify } from "./json-construction"
import { PROTOCOL_LIMITS, type JsonValue } from "./generated/protocol-3"
import type { StreamCredit } from "./stream-credit"

export const DEFAULT_MAX_RPC_LINE_BYTES = PROTOCOL_LIMITS.maxLineBytes

export class LineTooLargeError extends Error {
  constructor(readonly limit: number) {
    super(`JSON-RPC line exceeds the ${limit}-byte limit`)
    this.name = "LineTooLargeError"
  }
}

export class UnterminatedLineError extends Error {
  constructor() {
    super("JSON-RPC input ended before a newline terminator")
    this.name = "UnterminatedLineError"
  }
}

export async function* readBoundedLines(
  input: AsyncIterable<Uint8Array>,
  maxBytes = DEFAULT_MAX_RPC_LINE_BYTES,
): AsyncGenerator<string> {
  const decoder = new TextDecoder("utf-8", { fatal: true })
  let buffered = new Uint8Array(Math.min(4096, maxBytes + 1))
  let length = 0
  for await (const chunk of input) {
    for (const byte of chunk) {
      if (byte === 0x0a) {
        if (length > 0 && buffered[length - 1] === 0x0d) length -= 1
        if (length > 0) yield decoder.decode(buffered.subarray(0, length))
        length = 0
      } else {
        if (length >= maxBytes) throw new LineTooLargeError(maxBytes)
        if (length === buffered.length) {
          const expanded = new Uint8Array(Math.min(maxBytes, Math.max(1, buffered.length * 2)))
          expanded.set(buffered)
          buffered = expanded
        }
        buffered[length] = byte
        length += 1
      }
    }
  }
  if (length > 0) throw new UnterminatedLineError()
}

export interface RpcOutput {
  write(line: Uint8Array): Promise<void> | void
}

export class OutboundQueueFullError extends Error {
  constructor() {
    super("JSON-RPC outbound queue capacity exceeded")
    this.name = "OutboundQueueFullError"
  }
}

export interface JsonWriterOptions {
  readonly maxQueuedBytes?: number
  readonly maxQueuedFrames?: number
  readonly writeTimeoutMs?: number
  readonly onFailure?: (error: Error) => void
}

interface PendingWrite {
  readonly priority: "control" | "progress" | "data"
  readonly bytes: Uint8Array
  readonly resolve: () => void
  readonly reject: (error: Error) => void
}

export class BoundedJsonWriter {
  readonly #encoder = new TextEncoder()
  #constructing = false
  readonly #queue: PendingWrite[] = []
  readonly #dataQueue: PendingWrite[] = []
  readonly #creditQueue = new Map<PendingWrite, StreamCredit>()
  readonly #progressQueue: PendingWrite[] = []
  #progressBytes = 0
  #dataBytes = 0
  readonly #drainers: Array<{ resolve: () => void; reject: (error: Error) => void }> = []
  #active: PendingWrite | undefined
  #queuedBytes = 0
  #error: Error | undefined
  #timeout: ReturnType<typeof setTimeout> | undefined
  readonly #maxQueuedBytes: number
  readonly #maxQueuedFrames: number
  readonly #writeTimeoutMs: number
  readonly #onFailure: ((error: Error) => void) | undefined

  constructor(
    private readonly output: RpcOutput,
    private readonly maxBytes = DEFAULT_MAX_RPC_LINE_BYTES,
    options: JsonWriterOptions = {},
  ) {
    this.#maxQueuedBytes = options.maxQueuedBytes ?? PROTOCOL_LIMITS.controlQueueBytes
    this.#maxQueuedFrames = options.maxQueuedFrames ?? PROTOCOL_LIMITS.controlQueueFrames
    this.#writeTimeoutMs = options.writeTimeoutMs ?? 30_000
    this.#onFailure = options.onFailure
    for (const limit of [maxBytes, this.#maxQueuedBytes, this.#maxQueuedFrames, this.#writeTimeoutMs]) {
      if (!Number.isSafeInteger(limit) || limit < 1) throw new Error("writer limits must be positive integers")
    }
  }

  write(value: JsonValue, priority: "control" | "progress" | "data" = "control", credit?: StreamCredit): Promise<void> {
    if (credit !== undefined && priority !== "data") throw new TypeError("delivery credit requires a data frame")
    if (this.#error !== undefined) return Promise.reject(this.#error)
    // No user getter/toJSON or native enumeration runs after known exhaustion.
    if (this.#constructing) return Promise.reject(new OutboundQueueFullError())
    const queue = priority === "control" ? this.#queue : priority === "progress" ? this.#progressQueue : this.#dataQueue
    const active = this.#active?.priority === priority ? 1 : 0
    const queuedBytes = priority === "control" ? this.#queuedBytes : priority === "progress" ? this.#progressBytes : this.#dataBytes
    const maxBytes = priority === "control" ? this.#maxQueuedBytes
      : priority === "progress" ? PROTOCOL_LIMITS.maxInFlightRequests * (PROTOCOL_LIMITS.maxProgressFrameBytes + 1)
      : PROTOCOL_LIMITS.dataQueueBytes
    const maxFrames = priority === "control" ? this.#maxQueuedFrames
      : priority === "progress" ? PROTOCOL_LIMITS.maxInFlightRequests : PROTOCOL_LIMITS.maxProviderStreams
    const remaining = maxBytes - queuedBytes
    const waiting = priority === "data" ? this.#creditQueue.size : 0
    if (remaining < 2 || queue.length + active + waiting >= maxFrames) {
      const error = new OutboundQueueFullError()
      this.abort(error)
      return Promise.reject(error)
    }
    const lineLimit = priority === "progress" ? Math.min(this.maxBytes, PROTOCOL_LIMITS.maxProgressFrameBytes)
      : credit !== undefined ? Math.min(this.maxBytes, PROTOCOL_LIMITS.providerWindowBytes) : this.maxBytes
    const constructionLimit = Math.min(lineLimit, remaining - 1)
    let serialized: { readonly text: string; readonly bytes: number }
    this.#constructing = true
    try {
      serialized = boundedJsonStringify(value, constructionLimit, () =>
        constructionLimit < lineLimit ? new OutboundQueueFullError() : new LineTooLargeError(lineLimit))
    } catch (error) {
      if (error instanceof OutboundQueueFullError) this.abort(error)
      if (error instanceof OutboundQueueFullError || error instanceof LineTooLargeError) return Promise.reject(error)
      throw error
    } finally {
      this.#constructing = false
    }
    if (this.#error !== undefined) return Promise.reject(this.#error)
    // The exact single output buffer is admitted before allocation. The bounded
    // native UTF-16 construction string is synchronous scratch, never queued.
    const size = serialized.bytes + 1
    if (priority === "control") this.#queuedBytes += size
    else if (priority === "progress") this.#progressBytes += size
    else this.#dataBytes += size
    let bytes: Uint8Array
    try {
      bytes = new Uint8Array(size)
      const encoded = this.#encoder.encodeInto(serialized.text, bytes)
      if (encoded.read !== serialized.text.length || encoded.written !== serialized.bytes) throw new Error("JSON-RPC construction size mismatch")
      bytes[serialized.bytes] = 0x0a
    } catch (error) {
      this.abort(error instanceof Error ? error : new Error("JSON-RPC construction failed"))
      return Promise.reject(this.#error)
    }
    const pending = new Promise<void>((resolve, reject) => {
      const item: PendingWrite = { bytes, priority, resolve, reject }
      if (credit === undefined) queue.push(item)
      else {
        // The exact encoded frame owns queue capacity while waiting. Control
        // replies stay live; no second serialization can change its byte debit.
        this.#creditQueue.set(item, credit)
        void credit.take(size - 1).then(() => {
          if (!this.#creditQueue.delete(item)) return
          queue.push(item)
          this.#pump()
        }, error => {
          if (!this.#creditQueue.delete(item)) return
          this.#dataBytes -= size
          reject(error)
          this.#pump()
        })
      }
    })
    this.#pump()
    return pending
  }

  drain(): Promise<void> {
    if (this.#error !== undefined) return Promise.reject(this.#error)
    if (this.#active === undefined && this.#creditQueue.size === 0) return Promise.resolve()
    return new Promise<void>((resolve, reject) => this.#drainers.push({ resolve, reject }))
  }

  abort(error: Error): void {
    if (this.#error !== undefined) return
    this.#error = error
    this.#active?.reject(error)
    this.#active = undefined
    for (const item of [...this.#queue.splice(0), ...this.#progressQueue.splice(0), ...this.#dataQueue.splice(0)]) item.reject(error)
    for (const [item, credit] of this.#creditQueue) {
      item.reject(error)
      credit.close(error)
    }
    this.#creditQueue.clear()
    this.#queuedBytes = 0
    this.#dataBytes = 0
    this.#progressBytes = 0
    for (const waiter of this.#drainers.splice(0)) waiter.reject(error)
    if (this.#timeout !== undefined) clearTimeout(this.#timeout)
    this.#onFailure?.(error)
  }

  #pump(): void {
    if (this.#active !== undefined || this.#error !== undefined) return
    const item = this.#queue.shift() ?? this.#progressQueue.shift() ?? this.#dataQueue.shift()
    if (item === undefined) {
      if (this.#creditQueue.size === 0) for (const waiter of this.#drainers.splice(0)) waiter.resolve()
      return
    }
    this.#active = item
    this.#timeout = setTimeout(() => this.abort(new Error("JSON-RPC output write timed out")), this.#writeTimeoutMs)
    void Promise.resolve().then(() => {
      if (this.#active === item) return this.output.write(item.bytes)
    }).then(() => {
      if (this.#active !== item) return
      if (this.#timeout !== undefined) clearTimeout(this.#timeout)
      this.#active = undefined
      if (item.priority === "control") this.#queuedBytes -= item.bytes.byteLength
      else if (item.priority === "progress") this.#progressBytes -= item.bytes.byteLength
      else this.#dataBytes -= item.bytes.byteLength
      item.resolve()
      this.#pump()
    }, () => this.abort(new Error("JSON-RPC output write failed")))
  }
}

export async function* readableStreamBytes(
  stream: ReadableStream<Uint8Array>,
  signal?: AbortSignal,
): AsyncGenerator<Uint8Array> {
  const reader = stream.getReader()
  const abort = () => void reader.cancel().catch(() => undefined)
  signal?.addEventListener("abort", abort, { once: true })
  try {
    while (true) {
      const next = await reader.read()
      if (next.done) return
      yield next.value
    }
  } finally {
    signal?.removeEventListener("abort", abort)
    reader.releaseLock()
  }
}
