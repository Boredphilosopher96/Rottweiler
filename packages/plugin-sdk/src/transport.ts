import { PROTOCOL_LIMITS, type JsonValue } from "./generated/protocol-2"

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

export class BoundedJsonWriter {
  readonly #encoder = new TextEncoder()
  #tail = Promise.resolve()

  constructor(
    private readonly output: RpcOutput,
    private readonly maxBytes = DEFAULT_MAX_RPC_LINE_BYTES,
  ) {}

  write(value: JsonValue): Promise<void> {
    const serialized = JSON.stringify(value)
    if (serialized === undefined) return Promise.reject(new TypeError("JSON-RPC value is not serializable"))
    const payload = this.#encoder.encode(serialized)
    if (payload.byteLength > this.maxBytes) {
      return Promise.reject(new LineTooLargeError(this.maxBytes))
    }
    const encoded = this.#encoder.encode(`${serialized}\n`)
    const write = async () => this.output.write(encoded)
    const next = this.#tail.then(write, write)
    this.#tail = next.catch(() => undefined)
    return next
  }

  drain(): Promise<void> {
    return this.#tail
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
