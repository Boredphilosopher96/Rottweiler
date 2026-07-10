import { describe, expect, test } from "bun:test"

import { SseLimitError, SseParser, backoffDelay } from "../src/transport"

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
})

test("reconnect backoff is deterministic, exponential, and capped", () => {
  const policy = { initialDelayMs: 100, maximumDelayMs: 350, multiplier: 2 }
  expect([0, 1, 2, 3].map((attempt) => backoffDelay(policy, attempt))).toEqual([
    100, 200, 350, 350,
  ])
})
