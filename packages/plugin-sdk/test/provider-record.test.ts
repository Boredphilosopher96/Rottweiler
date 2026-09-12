import { expect, test } from "bun:test"
import { readFileSync } from "node:fs"
import validateProviderEvent, { PROVIDER_EVENT_FIELDS } from "../src/generated/provider-event-validator.js"
import { PROTOCOL_LIMITS, type ProviderEvent } from "../src/generated/protocol-3"
import { captureProviderEvent } from "../src/provider-event"
import { PluginServer } from "../src/server"

const until = async (done: () => boolean): Promise<void> => {
  const deadline = Date.now() + 2_000
  while (!done()) {
    if (Date.now() >= deadline) throw new Error("provider fixture did not settle")
    await Bun.sleep(1)
  }
}

test("capture field names come from the complete ProviderEvent schema", () => {
  const schema = JSON.parse(readFileSync(new URL("../fixtures/wire/provider-event.schema.json", import.meta.url), "utf8")) as {
    oneOf: Array<{ properties: Record<string, unknown> }>
  }
  const fields = [...new Set(schema.oneOf.flatMap(variant => Object.keys(variant.properties)))].sort()
  expect(PROVIDER_EVENT_FIELDS).toEqual(fields)
  expect(Object.isFrozen(PROVIDER_EVENT_FIELDS)).toBe(true)
})

test("capture reads each field once and keeps nested JSON references", () => {
  let reads = 0
  const argumentsValue = { values: [null, false, "λ", { exact: 7 }] }
  const event = { get type() { reads += 1; return "tool_call_end" }, id: "tool", arguments: argumentsValue }
  const captured = captureProviderEvent(event)
  expect(reads).toBe(1)
  expect(captured?.type).toBe("tool_call_end")
  if (captured?.type !== "tool_call_end") throw new Error("wrong event")
  expect(captured.arguments).toBe(argumentsValue)
  expect(JSON.stringify(captured)).toBe('{"type":"tool_call_end","id":"tool","arguments":{"values":[null,false,"λ",{"exact":7}]}}')
  expect(Object.getPrototypeOf(captured)).toBeNull()
  expect(Object.isFrozen(captured)).toBe(true)
  expect(reads).toBe(1)
})

test("unknown fields and inherited protocol data cannot grow a copied event", () => {
  let visited = false
  expect(captureProviderEvent({ get unknown() { visited = true; return "x".repeat(1_000_000) }, type: "finished", reason: "stop" })).toBeUndefined()
  expect(visited).toBe(false)
  expect(captureProviderEvent(Object.create({ type: "finished", reason: "stop" }))).toBeUndefined()
  expect(captureProviderEvent(new Date())).toBeUndefined()
  const hidden = { reason: "stop" }
  Object.defineProperty(hidden, "type", { value: "finished", enumerable: false })
  expect(captureProviderEvent(hidden)).toBeUndefined()
  const plain = Object.assign(Object.create(null), { type: "finished", reason: "stop" })
  expect(captureProviderEvent(plain)).toEqual(plain)
})

test("changing event tags cannot bypass credit or counterfeit stream completion", async () => {
  let validationReads = 0
  expect(validateProviderEvent({ get type() { validationReads += 1; return "text_delta" }, text: "probe" })).toBe(true)
  let reads = 0
  const event = {
    get type() {
      reads += 1
      return reads <= validationReads ? "text_delta" : reads <= validationReads + 2 ? "finished" : "text_delta"
    },
    text: "exact output",
  } as ProviderEvent
  const frames: Array<Record<string, unknown>> = []
  const server = new PluginServer({
    manifest: { name: "probe", version: "1.0.0", protocol: 3, capabilities: { providers: [{ "alias-prefix": "probe/" }] } },
    handlers: { providers: { "probe/": async function* () { yield event; yield { type: "finished", reason: "stop" } } } },
  }, {
    input: (async function* () {})(),
    output: { write(bytes) { frames.push(JSON.parse(new TextDecoder().decode(bytes)) as Record<string, unknown>) } },
    error: { write() {} },
  })
  const send = (value: unknown) => server.handleLine(JSON.stringify(value))
  await send({ jsonrpc: "2.0", id: 1, method: "initialize", params: { host: "rottweiler", protocol: 3, max_frame_bytes: PROTOCOL_LIMITS.maxLineBytes } })
  try {
    await send({ jsonrpc: "2.0", id: 2, method: "provider/complete", params: { alias: "probe/model", request: {
      model: "model", turns: [], tools: [], tool_choice: { mode: "auto" }, output: { mode: "text" }, max_output_tokens: 64,
      cache_hint: null, temperature: null, thinking: "off",
    } } })
    await until(() => reads > 0)
    await send({ jsonrpc: "2.0", id: 3, method: "unknown" })
    await until(() => frames.some(frame => frame.id === 3))
    expect(reads).toBe(1)
    expect(frames.some(frame => frame.method === "provider/event")).toBe(false)
    expect(frames.some(frame => frame.id === 2)).toBe(false)
    const expected = { jsonrpc: "2.0", method: "provider/event", params: { request_id: 2, event: { type: "text_delta", text: "exact output" } } }
    await send({ jsonrpc: "2.0", method: "provider/credit", params: { request_id: 2, events: 1, bytes: Buffer.byteLength(JSON.stringify(expected)) } })
    await until(() => frames.some(frame => frame.id === 2))
    expect(frames.filter(frame => frame.method === "provider/event")).toEqual([
      expected,
      { jsonrpc: "2.0", method: "provider/event", params: { request_id: 2, event: { type: "finished", reason: "stop" } } },
    ])
    expect(frames.find(frame => frame.id === 2)).toEqual({ jsonrpc: "2.0", id: 2, result: null })
    expect(reads).toBe(1)
  } finally { await send({ jsonrpc: "2.0", id: 4, method: "shutdown", params: {} }) }
})
