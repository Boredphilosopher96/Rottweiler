import { expect, spyOn, test } from "bun:test"
import { ClientCache } from "../src/history/cache"
import { CacheRead } from "../src/history/read-allocation"
import { retainedJsonBytes } from "../src/retained-json"
import { boundedJson } from "../src/transport/json"
import { JsonAllocationShape } from "../src/transport/reply-allocation"

const examples: unknown[] = [null, true, 0, [], {}, "🐕\\\"", [1, 2, false, null],
  { a: "value", nested: [{ text: "λ".repeat(200) }], quote: '\\"[,{}]:"' },
  Array.from({ length: 500 }, (_, index) => ({ [String(index)]: [null, false, index] })),
]

test("allocation shape bounds retained graphs identically across every byte boundary", () => {
  for (const value of examples) {
    const bytes = new TextEncoder().encode(JSON.stringify(value))
    const whole = new JsonAllocationShape()
    whole.append(bytes)
    const fragmented = new JsonAllocationShape()
    for (const byte of bytes) fragmented.append(Uint8Array.of(byte))
    expect(fragmented.peak(bytes.length, 0)).toBe(whole.peak(bytes.length, 0))
    expect(whole.peak(bytes.length, 0)).toBeGreaterThanOrEqual(retainedJsonBytes(value, 1_000_000) + bytes.length * 3)
  }
})

test("transport admits source collection and decoding before parsing, then transfers the same cache owner", async () => {
  const value = { rows: Array.from({ length: 100 }, (_, index) => ({ id: String(index), text: "🐕".repeat(20) })) }
  const bytes = new TextEncoder().encode(JSON.stringify(value))
  const cache = new ClientCache<unknown>({ bytes: 256_000, entries: 4 })
  const incoming = new CacheRead(cache)
  let parsed = 0
  const original = JSON.parse
  const parse = spyOn(JSON, "parse").mockImplementation(text => {
    parsed++
    expect(cache.usage.bytes).toBeGreaterThanOrEqual(retainedJsonBytes(value, 256_000) + bytes.length * 3)
    return original(text)
  })
  const response = new Response(new ReadableStream<Uint8Array>({
    start(controller) {
      for (let offset = 0; offset < bytes.length; offset += 7) controller.enqueue(bytes.subarray(offset, offset + 7))
      controller.close()
    },
  }))
  try {
    const decoded = await boundedJson(response, bytes.length, undefined, undefined, incoming)
    const lease = incoming.commit("result", decoded)
    incoming.release()
    expect(parsed).toBe(1)
    expect(lease.value).toEqual(value)
    expect(cache.usage.pinnedEntries).toBe(1)
    cache.clear()
    expect(cache.usage.bytes).toBeGreaterThan(0)
    lease.release()
    expect(cache.usage.bytes).toBe(0)
  } finally { parse.mockRestore(); incoming.release() }
})

test("rejected decode cancels its body before releasing the still-owned read charge", async () => {
  const cache = new ClientCache<unknown>({ bytes: 8000, entries: 3 })
  const incoming = new CacheRead(cache)
  let cancelled = false
  let settle!: () => void
  const response = new Response(new ReadableStream<Uint8Array>({
    start(controller) { controller.enqueue(new TextEncoder().encode(JSON.stringify({ text: "x".repeat(2000) }))) },
    cancel() { cancelled = true; return new Promise<void>(resolve => { settle = resolve }) },
  }))
  const parse = spyOn(JSON, "parse")
  const pending = boundedJson(response, 4096, undefined, undefined, incoming).finally(() => incoming.release())
  await Promise.resolve(); await Promise.resolve()
  expect(cancelled).toBe(true)
  expect(cache.usage.bytes).toBeGreaterThan(0)
  expect(parse).not.toHaveBeenCalled()
  settle()
  await expect(pending).rejects.toThrow("allowance")
  parse.mockRestore()
  expect(cache.usage.bytes).toBe(0)
})

test("JSON depth is checked before the native decoder can allocate a deep graph", async () => {
  const incoming = new CacheRead(new ClientCache<unknown>())
  try {
    await expect(boundedJson(new Response("[".repeat(65) + "0" + "]".repeat(65)), 1000, undefined, undefined, incoming)).rejects.toThrow("nesting")
  } finally { incoming.release() }
})
