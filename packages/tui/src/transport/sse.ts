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
  readonly #encoder = new TextEncoder()
  readonly #maxLineBytes: number
  readonly #maxDataBytes: number
  #line: number[] = []
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
    for (const byte of chunk) {
      if (byte === 0x0a) {
        this.#consumeLine(messages)
        continue
      }
      this.#line.push(byte)
      if (this.#line.length > this.#maxLineBytes) {
        throw new SseLimitError("SSE line exceeds configured byte limit")
      }
    }
    return messages
  }

  finish(): SseMessage[] {
    const messages: SseMessage[] = []
    if (this.#line.length > 0) {
      this.#consumeLine(messages)
    }
    this.#dispatch(messages)
    return messages
  }

  #consumeLine(messages: SseMessage[]): void {
    if (this.#line.at(-1) === 0x0d) {
      this.#line.pop()
    }
    const line = this.#decoder.decode(Uint8Array.from(this.#line))
    this.#line = []

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
    if (value.startsWith(" ")) {
      value = value.slice(1)
    }

    switch (field) {
      case "data": {
        const separatorBytes = this.#data.length === 0 ? 0 : 1
        const valueBytes = this.#encoder.encode(value).byteLength
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
