import { expect, spyOn, test } from "bun:test"
import { boundedJsonStringify, MAX_JSON_CONSTRUCTION_DEPTH } from "../src/json-construction"
import { BoundedJsonWriter, LineTooLargeError, OutboundQueueFullError } from "../src/transport"
import type { JsonValue } from "../src/generated/protocol-3"

function compare(value: unknown): void {
  const expected = JSON.stringify(value)
  const actual = boundedJsonStringify(value as JsonValue, 1024 * 1024, () => new Error("limit"))
  expect(actual.text).toBe(expected)
  expect(actual.bytes).toBe(Buffer.byteLength(expected!))
}

test("bounded native construction preserves exact JSON scalar, object and array semantics", () => {
  for (const value of [null, true, false, -0, 1e25, 1e-7, NaN, Infinity,
    '"\\\b\f\n\r\t\u0000\u001f é中😀\ud800\udfff',
    { "é\ud800": "value", 10: "ten", 2: "two", missing: undefined, ignored: Symbol("ignored") },
    [undefined, , Symbol("hole"), NaN, () => 1],
    new Number(3), new String("boxed"), new Boolean(false), Object(Symbol("empty")),
    { toJSON(key: string) { return { key, payload: "custom" } } }, new Date("2026-09-08T00:00:00Z"),
    { left: { same: 1 }, right: { same: 1 } },
  ]) compare(value)
  const shared = { value: "shared" }
  compare([shared, shared])
  for (let code = 0; code <= 0xffff; code += 127) compare(String.fromCharCode(code, code ^ 0xffff))
})

test("getters, toJSON and boxed primitive coercion are each evaluated once", () => {
  let getterCalls = 0
  let toJsonCalls = 0
  let primitiveCalls = 0
  const boxed = new Number(3)
  boxed.valueOf = () => { primitiveCalls += 1; return 7 }
  const input = {
    get value() { getterCalls += 1; return { toJSON() { toJsonCalls += 1; return boxed } } },
  }
  const result = boundedJsonStringify(input as unknown as JsonValue, 100, () => new Error("limit"))
  expect(result.text).toBe('{"value":7}')
  expect([getterCalls, toJsonCalls, primitiveCalls]).toEqual([1, 1, 1])
})

test("exact UTF-8 admission precedes escaping and encoded allocation", async () => {
  const encode = spyOn(TextEncoder.prototype, "encodeInto")
  let later = 0
  try {
    const writer = new BoundedJsonWriter({ write() { throw new Error("not admitted") } }, 64)
    const input = { large: "\u0000".repeat(4 * 1024 * 1024), get later() { later += 1; return 1 } }
    await expect(writer.write(input)).rejects.toBeInstanceOf(LineTooLargeError)
    expect(later).toBe(0)
    expect(encode).not.toHaveBeenCalled()
    const output: Uint8Array[] = []
    const exact = new BoundedJsonWriter({ write(bytes) { output.push(bytes) } }, 6)
    await exact.write("😀")
    expect(encode).toHaveBeenCalledTimes(1)
    expect(output[0]?.byteLength).toBe(7)
    expect(new TextDecoder().decode(output[0])).toBe('"😀"\n')
  } finally { encode.mockRestore() }
})

test("saturated frame admission never starts enumeration, getters or toJSON", async () => {
  let release!: () => void
  let visited = 0
  const writer = new BoundedJsonWriter({ write: () => new Promise<void>(resolve => { release = resolve }) }, 64,
    { maxQueuedBytes: 64, maxQueuedFrames: 1 })
  const first = writer.write(null)
  const pending = first.catch(error => error)
  await Promise.resolve()
  const input = { toJSON() { visited += 1; throw new Error("must not visit") } }
  await expect(writer.write(input as unknown as JsonValue)).rejects.toBeInstanceOf(OutboundQueueFullError)
  expect(visited).toBe(0)
  release()
  expect(await pending).toBeInstanceOf(OutboundQueueFullError)
})

test("remaining aggregate bytes bound construction before allocating a rejected frame", async () => {
  let release!: () => void
  const writer = new BoundedJsonWriter({ write: () => new Promise<void>(resolve => { release = resolve }) }, 64,
    { maxQueuedBytes: 8, maxQueuedFrames: 4 })
  const first = writer.write("a").catch(error => error) // Four bytes held in actual output.
  await Promise.resolve()
  const encode = spyOn(TextEncoder.prototype, "encodeInto")
  let late = 0
  try {
    await expect(writer.write({ first: "large", get late() { late += 1; return 1 } })).rejects.toBeInstanceOf(OutboundQueueFullError)
    expect(late).toBe(0)
    expect(encode).not.toHaveBeenCalled()
    release()
    expect(await first).toBeInstanceOf(OutboundQueueFullError)
  } finally { encode.mockRestore() }
})

test("bounded traversal preserves native cycle and BigInt errors and caps ancestor storage", () => {
  const cycle: Record<string, unknown> = {}; cycle.self = cycle
  for (const input of [cycle, 1n, Object(1n)]) {
    expect(() => boundedJsonStringify(input as JsonValue, 1024, () => new Error("limit"))).toThrow(TypeError)
  }
  let nested: JsonValue = null
  for (let index = 0; index < MAX_JSON_CONSTRUCTION_DEPTH; index += 1) nested = [nested]
  compare(nested)
  expect(() => boundedJsonStringify([nested], 1024, () => new Error("limit"))).toThrow("nesting exceeded")
})

test("small frame output uses one exact backing buffer without duplicate full encoding", async () => {
  const encode = spyOn(TextEncoder.prototype, "encode")
  const encodeInto = spyOn(TextEncoder.prototype, "encodeInto")
  const outputs: Uint8Array[] = []
  try {
    const writer = new BoundedJsonWriter({ write(bytes) { outputs.push(bytes) } })
    await writer.write({ value: "é😀" })
    expect(encode).not.toHaveBeenCalled()
    expect(encodeInto).toHaveBeenCalledTimes(1)
    const expected = JSON.stringify({ value: "é😀" }) + "\n"
    expect(outputs[0]?.byteLength).toBe(Buffer.byteLength(expected))
    expect(outputs[0]?.buffer.byteLength).toBe(outputs[0]?.byteLength)
    expect(Buffer.from(outputs[0]!).toString()).toBe(expected)
  } finally { encode.mockRestore(); encodeInto.mockRestore() }
})


test("flat object traversal stops before later values while native key enumeration remains VM-owned", () => {
  let visited = 0
  const value: Record<string, unknown> = {}
  for (let index = 0; index < 10_000; index += 1) {
    Object.defineProperty(value, `key${index}`, { enumerable: true, get() { visited += 1; return "x" } })
  }
  expect(() => boundedJsonStringify(value as JsonValue, 64, () => new Error("limit"))).toThrow("limit")
  expect(visited).toBeGreaterThan(0)
  expect(visited).toBeLessThan(10)
})

test("reentrant serialization cannot create another construction in the same writer", async () => {
  const output: string[] = []
  let nested: Promise<unknown> | undefined
  const writer = new BoundedJsonWriter({ write(bytes) { output.push(Buffer.from(bytes).toString()) } })
  const value = { toJSON() { nested = writer.write("nested").catch(error => error); return "outer" } }
  await writer.write(value as unknown as JsonValue)
  expect(await nested).toBeInstanceOf(OutboundQueueFullError)
  expect(output).toEqual(['"outer"\n'])
})
