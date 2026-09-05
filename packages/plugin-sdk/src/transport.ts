import { PROTOCOL_LIMITS, type JsonValue } from "./generated/protocol-3"

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
  readonly priority: "control" | "data"
  readonly bytes: Uint8Array
  readonly resolve: () => void
  readonly reject: (error: Error) => void
}

export class BoundedJsonWriter {
  readonly #encoder = new TextEncoder()
  readonly #queue: PendingWrite[] = []
  readonly #dataQueue: PendingWrite[] = []
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

  write(value: JsonValue, priority: "control" | "data" = "control"): Promise<void> {
    if (this.#error !== undefined) return Promise.reject(this.#error)
    const serialized = JSON.stringify(value)
    if (serialized === undefined) return Promise.reject(new TypeError("JSON-RPC value is not serializable"))
    const payload = this.#encoder.encode(serialized)
    if (payload.byteLength > this.maxBytes) {
      return Promise.reject(new LineTooLargeError(this.maxBytes))
    }
    const bytes = this.#encoder.encode(`${serialized}\n`)
    const queue = priority === "control" ? this.#queue : this.#dataQueue
    const active = this.#active?.priority === priority ? 1 : 0
    const queuedBytes = priority === "control" ? this.#queuedBytes : this.#dataBytes
    const maxBytes = priority === "control" ? this.#maxQueuedBytes
      : PROTOCOL_LIMITS.dataQueueBytes
    const maxFrames = priority === "control" ? this.#maxQueuedFrames : PROTOCOL_LIMITS.maxProviderStreams
    if (queuedBytes + bytes.byteLength > maxBytes || queue.length + active >= maxFrames) {
      const error = new OutboundQueueFullError()
      this.abort(error)
      return Promise.reject(error)
    }
    if (priority === "control") this.#queuedBytes += bytes.byteLength
    else this.#dataBytes += bytes.byteLength
    const pending = new Promise<void>((resolve, reject) => queue.push({ bytes, priority, resolve, reject }))
    this.#pump()
    return pending
  }

  drain(): Promise<void> {
    if (this.#error !== undefined) return Promise.reject(this.#error)
    if (this.#active === undefined) return Promise.resolve()
    return new Promise<void>((resolve, reject) => this.#drainers.push({ resolve, reject }))
  }

  abort(error: Error): void {
    if (this.#error !== undefined) return
    this.#error = error
    this.#active?.reject(error)
    this.#active = undefined
    for (const item of [...this.#queue.splice(0), ...this.#dataQueue.splice(0)]) item.reject(error)
    this.#queuedBytes = 0
    this.#dataBytes = 0
    for (const waiter of this.#drainers.splice(0)) waiter.reject(error)
    if (this.#timeout !== undefined) clearTimeout(this.#timeout)
    this.#onFailure?.(error)
  }

  #pump(): void {
    if (this.#active !== undefined || this.#error !== undefined) return
    const item = this.#queue.shift() ?? this.#dataQueue.shift()
    if (item === undefined) {
      for (const waiter of this.#drainers.splice(0)) waiter.resolve()
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
