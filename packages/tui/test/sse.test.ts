import { describe, expect, test } from "bun:test"

import { SseLimitError, SseParser, backoffDelay, parseSseStream } from "../src/transport"

const encoder = new TextEncoder()

describe("bounded SSE parser", () => {
  test("survives arbitrary chunk boundaries and joins multiline data", () => {
    const parser = new SseParser()
    const bytes = encoder.encode(
      ": keepalive\r\nevent: fixture\r\nid: 9\r\nretry: 25\r\ndata: first\r\ndata: second\r\n\r\n",
    )
    const messages = []
    for (const byte of bytes) {
      messages.push(...parser.push(Uint8Array.of(byte)))
    }
    messages.push(...parser.finish())

    expect(messages).toEqual([
      {
        event: "fixture",
        id: "9",
        retry: 25,
        data: "first\nsecond",
      },
    ])
  })

  test("accepts a bounded event larger than the old 64 KiB line limit", () => {
    const parser = new SseParser()
    const payload = JSON.stringify({
      type: "conversation_turn_committed",
      turn: {
        role: "assistant",
        blocks: [{ type: "text", text: "x".repeat(96 * 1024) }],
      },
    })
    const messages = parser.push(encoder.encode(`data: ${payload}\n\n`))

    expect(messages).toHaveLength(1)
    expect(messages[0]?.data).toBe(payload)
  })

  test("accepts the base64 wire size of a legal 5 MiB image", () => {
    const parser = new SseParser()
    const base64Bytes = Math.ceil((5 * 1024 * 1024) / 3) * 4
    const payload = JSON.stringify({
      type: "conversation_turn_committed",
      turn: {
        role: "user",
        blocks: [
          {
            type: "image",
            media_type: "image/png",
            data: "x".repeat(base64Bytes),
          },
        ],
      },
    })

    const messages = parser.push(encoder.encode(`data: ${payload}\n\n`))
    expect(messages).toHaveLength(1)
    expect(messages[0]?.data.length).toBe(payload.length)
  })

  test("flushes a final complete field and rejects unbounded input", () => {
    const parser = new SseParser({ maxLineBytes: 16, maxDataBytes: 8 })
    expect(parser.push(encoder.encode("data: ok\n"))).toEqual([])
    expect(parser.finish()).toEqual([{ data: "ok" }])

    expect(() =>
      new SseParser({ maxLineBytes: 4 }).push(encoder.encode("data: too long")),
    ).toThrow(SseLimitError)
    expect(() =>
      new SseParser({ maxDataBytes: 3 }).push(encoder.encode("data: four\n\n")),
    ).toThrow(SseLimitError)
  })

  test("releases the reader lock even when underlying cancellation rejects", async () => {
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(encoder.encode("data: first\n\n"))
      },
      cancel() {
        return Promise.reject(new Error("fixture cancellation failed"))
      },
    })
    const messages = parseSseStream(stream)
    expect((await messages.next()).value).toEqual({ data: "first" })
    await expect(messages.return(undefined)).rejects.toThrow("fixture cancellation failed")

    const replacement = stream.getReader()
    replacement.releaseLock()
  })
})

test("reconnect backoff is deterministic, exponential, and capped", () => {
  const policy = { initialDelayMs: 100, maximumDelayMs: 350, multiplier: 2 }
  expect([0, 1, 2, 3].map((attempt) => backoffDelay(policy, attempt))).toEqual([
    100, 200, 350, 350,
  ])
})
