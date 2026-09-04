export interface SseMessage {
  readonly data: string
  readonly event?: string
  readonly id?: string
  readonly retry?: number
}

export interface SseParserOptions {
  readonly maxLineBytes?: number
  readonly maxDataBytes?: number
  readonly maxDataLines?: number
}

// A legal user turn can contain a 5 MiB decoded image, whose base64 wire form
// is nearly 7 MiB. The per-message attachment total is 10 MiB, so retain a
// bounded envelope with enough room for base64 expansion and JSON framing.
const DEFAULT_MAX_DATA_BYTES = 16 * 1024 * 1024
// The engine serializes each protocol event as one `data:` line. Keep the line
// bound aligned with the event-data bound plus the SSE field prefix.
const DEFAULT_MAX_LINE_BYTES = DEFAULT_MAX_DATA_BYTES + 16

/** A bounded, incremental parser for the event-stream wire format. */
export class SseParser {
  readonly #decoder = new TextDecoder()
  readonly #maxLineBytes: number
  readonly #maxDataBytes: number
  readonly #maxDataLines: number
  #lineLength = 0
  #copiedBytes = 0
  #allocatedBytes = 0
  #line = new Uint8Array()
  #data: string[] = []
  #dataBytes = 0
  #event: string | undefined
  #id: string | undefined
  #retry: number | undefined

  constructor(options: SseParserOptions = {}) {
    this.#maxLineBytes = positiveBound(options.maxLineBytes, DEFAULT_MAX_LINE_BYTES)
    this.#maxDataBytes = positiveBound(options.maxDataBytes, DEFAULT_MAX_DATA_BYTES)
    this.#maxDataLines = positiveBound(options.maxDataLines, 1024)
  }

  get storageWork(): { readonly copiedBytes: number; readonly allocatedBytes: number; readonly retainedBytes: number } {
    return { copiedBytes: this.#copiedBytes, allocatedBytes: this.#allocatedBytes, retainedBytes: this.#line.byteLength }
  }

  *push(chunk: Uint8Array): Generator<SseMessage> {
    let offset = 0
    while (offset < chunk.length) {
      const newline = chunk.indexOf(0x0a, offset)
      const end = newline < 0 ? chunk.length : newline
      const segment = chunk.subarray(offset, end)
      if (this.#lineLength + segment.length > this.#maxLineBytes) {
        throw new SseLimitError("SSE line exceeds configured byte limit")
      }
      if (newline < 0) {
        this.#appendLine(segment)
        break
      }
      let line = segment
      if (this.#lineLength > 0) {
        this.#appendLine(segment)
        line = this.#line.subarray(0, this.#lineLength)
      }
      const message = this.#consumeLine(line)
      this.#lineLength = 0
      if (this.#line.byteLength > 64 * 1024) this.#line = new Uint8Array()
      if (message !== null) yield message
      offset = newline + 1
    }
  }

  *finish(): Generator<SseMessage> {
    if (this.#lineLength > 0) {
      const message = this.#consumeLine(this.#line.subarray(0, this.#lineLength))
      if (message !== null) yield message
    }
    this.#line = new Uint8Array()
    this.#lineLength = 0
    const message = this.#dispatch()
    if (message !== null) yield message
  }

  #appendLine(segment: Uint8Array): void {
    const required = this.#lineLength + segment.length
    if (required > this.#line.length) {
      const capacity = Math.min(this.#maxLineBytes, Math.max(required, this.#line.length * 2, 1024))
      const expanded = new Uint8Array(capacity)
      expanded.set(this.#line.subarray(0, this.#lineLength))
      this.#copiedBytes += this.#lineLength
      this.#allocatedBytes += capacity
      this.#line = expanded
    }
    this.#line.set(segment, this.#lineLength)
    this.#lineLength = required
    this.#copiedBytes += segment.length
  }

  #consumeLine(bytes: Uint8Array): SseMessage | null {
    const lineBytes = bytes.at(-1) === 0x0d ? bytes.subarray(0, -1) : bytes
    const line = this.#decoder.decode(lineBytes)

    if (line.length === 0) {
      return this.#dispatch()
    }
    if (line.startsWith(":")) return null

    const colon = line.indexOf(":")
    const field = colon < 0 ? line : line.slice(0, colon)
    let value = colon < 0 ? "" : line.slice(colon + 1)
    const colonByte = lineBytes.indexOf(0x3a)
    let valueByteStart = colonByte < 0 ? lineBytes.length : colonByte + 1
    if (value.startsWith(" ")) {
      value = value.slice(1)
      valueByteStart += 1
    }

    switch (field) {
      case "data": {
        const separatorBytes = this.#data.length === 0 ? 0 : 1
        const valueBytes = lineBytes.length - valueByteStart
        this.#dataBytes += separatorBytes + valueBytes
        if (this.#data.length >= this.#maxDataLines) {
          throw new SseLimitError("SSE event data exceeds configured field limit")
        }
        if (this.#dataBytes > this.#maxDataBytes) {
          throw new SseLimitError("SSE event data exceeds configured byte limit")
        }
        this.#data.push(value)
        break
      }
      case "event":
        this.#event = value
        break
      case "id":
        if (!value.includes("\0")) {
          this.#id = value
        }
        break
      case "retry":
        if (/^\d+$/.test(value)) {
          const retry = Number(value)
          if (Number.isSafeInteger(retry)) {
            this.#retry = retry
          }
        }
        break
      default:
        break
    }
    return null
  }

  #dispatch(): SseMessage | null {
    const message: SseMessage | null = this.#data.length === 0 ? null : {
        data: this.#data.join("\n"),
        ...(this.#event === undefined ? {} : { event: this.#event }),
        ...(this.#id === undefined ? {} : { id: this.#id }),
        ...(this.#retry === undefined ? {} : { retry: this.#retry }),
      }
    this.#data = []
    this.#dataBytes = 0
    this.#event = undefined
    this.#id = undefined
    this.#retry = undefined
    return message
  }
}

export class SseLimitError extends Error {
  constructor(message: string) {
    super(message)
    this.name = "SseLimitError"
  }
}

export async function* parseSseStream(
  stream: ReadableStream<Uint8Array>,
  options: SseParserOptions = {},
  signal?: AbortSignal,
): AsyncGenerator<SseMessage> {
  const parser = new SseParser(options)
  const reader = stream.getReader()
  let cancellation: Promise<void> | null = null
  let completed = false
  const cancelReader = (reason: unknown): Promise<void> => {
    cancellation ??= reader.cancel(reason)
    return cancellation
  }
  const onAbort = () => {
    void cancelReader(signal?.reason)
  }
  if (signal?.aborted === true) {
    await cancelReader(signal.reason)
    reader.releaseLock()
    return
  } else {
    signal?.addEventListener("abort", onAbort, { once: true })
  }
  try {
    while (true) {
      const { done, value } = await reader.read()
      if (done) {
        completed = true
        break
      }
      for (const message of parser.push(value)) {
        yield message
      }
    }
    for (const message of parser.finish()) {
      yield message
    }
  } finally {
    signal?.removeEventListener("abort", onAbort)
    try {
      if (!completed) {
        await cancelReader(new Error("SSE consumer stopped before stream completion"))
      } else if (cancellation !== null) {
        await cancellation
      }
    } finally {
      // A rejecting underlying cancel must not strand the reader lock (or the
      // fetch body and its Unix socket) after an early consumer exit.
      reader.releaseLock()
    }
  }
}

function positiveBound(value: number | undefined, fallback: number): number {
  if (value === undefined) {
    return fallback
  }
  if (!Number.isSafeInteger(value) || value <= 0) {
    throw new RangeError("SSE parser limits must be positive safe integers")
  }
  return value
}
