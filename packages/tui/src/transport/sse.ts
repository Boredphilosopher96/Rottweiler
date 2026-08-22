export interface SseMessage {
  readonly data: string
  readonly event?: string
  readonly id?: string
  readonly retry?: number
}

export interface SseParserOptions {
  readonly maxLineBytes?: number
  readonly maxDataBytes?: number
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
  #line = new Uint8Array()
  #data: string[] = []
  #dataBytes = 0
  #event: string | undefined
  #id: string | undefined
  #retry: number | undefined

  constructor(options: SseParserOptions = {}) {
    this.#maxLineBytes = positiveBound(options.maxLineBytes, DEFAULT_MAX_LINE_BYTES)
    this.#maxDataBytes = positiveBound(options.maxDataBytes, DEFAULT_MAX_DATA_BYTES)
  }

  push(chunk: Uint8Array): SseMessage[] {
    const messages: SseMessage[] = []
    let offset = 0
    while (offset < chunk.length) {
      const newline = chunk.indexOf(0x0a, offset)
      const end = newline < 0 ? chunk.length : newline
      const segment = chunk.subarray(offset, end)
      if (this.#line.length + segment.length > this.#maxLineBytes) {
        throw new SseLimitError("SSE line exceeds configured byte limit")
      }
      if (newline < 0) {
        this.#line = appendBytes(this.#line, segment)
        break
      }
      const line = this.#line.length === 0 ? segment : appendBytes(this.#line, segment)
      this.#line = new Uint8Array()
      this.#consumeLine(line, messages)
      offset = newline + 1
    }
    return messages
  }

  finish(): SseMessage[] {
    const messages: SseMessage[] = []
    if (this.#line.length > 0) {
      this.#consumeLine(this.#line, messages)
      this.#line = new Uint8Array()
    }
    this.#dispatch(messages)
    return messages
  }

  #consumeLine(bytes: Uint8Array, messages: SseMessage[]): void {
    const lineBytes = bytes.at(-1) === 0x0d ? bytes.subarray(0, -1) : bytes
    const line = this.#decoder.decode(lineBytes)

    if (line.length === 0) {
      this.#dispatch(messages)
      return
    }
    if (line.startsWith(":")) {
      return
    }

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
  }

  #dispatch(messages: SseMessage[]): void {
    if (this.#data.length > 0) {
      messages.push({
        data: this.#data.join("\n"),
        ...(this.#event === undefined ? {} : { event: this.#event }),
        ...(this.#id === undefined ? {} : { id: this.#id }),
        ...(this.#retry === undefined ? {} : { retry: this.#retry }),
      })
    }
    this.#data = []
    this.#dataBytes = 0
    this.#event = undefined
    this.#id = undefined
    this.#retry = undefined
  }
}

function appendBytes(prefix: Uint8Array, suffix: Uint8Array): Uint8Array<ArrayBuffer> {
  if (prefix.length === 0) return suffix.slice()
  if (suffix.length === 0) return prefix.slice()
  const combined = new Uint8Array(prefix.length + suffix.length)
  combined.set(prefix)
  combined.set(suffix, prefix.length)
  return combined
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
