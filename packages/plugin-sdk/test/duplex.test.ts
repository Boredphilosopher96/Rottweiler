import { describe, expect, test } from "bun:test"
import type { ProviderRequest } from "../src/generated/provider-contract"

import {
  BoundedJsonWriter,
  OutboundQueueFullError,
  PluginServer,
  PROTOCOL_LIMITS,
  readableStreamBytes,
  type JsonValue,
  type PluginDefinition,
  type ServerTransport,
} from "../src/index"
const providerRequest: ProviderRequest = {
  model: "model", turns: [], tools: [], tool_choice: { mode: "auto" },
  max_output_tokens: 64, temperature: null, thinking: "off", cache_hint: null,
}


const encoder = new TextEncoder()
const decoder = new TextDecoder()
const initialize = {
  jsonrpc: "2.0", id: 1, method: "initialize",
  params: {
    host: "rottweiler", protocol: 3, max_frame_bytes: PROTOCOL_LIMITS.maxLineBytes,
    capabilities: ["provider-models", "provider-http"],
  },
} satisfies JsonValue
const models = { jsonrpc: "2.0", id: 2, method: "provider/models", params: { alias_prefix: "probe/" } } satisfies JsonValue
const stop = { jsonrpc: "2.0", id: 999, method: "shutdown" } satisfies JsonValue
const httpRequest = { method: "GET", url: "https://example.com/models", credential_header: "authorization" } as const

function fixture(): PluginDefinition {
  return {
    manifest: {
      name: "duplex", version: "1", protocol: 3,
      capabilities: {
        providers: [{ "alias-prefix": "probe/", capabilities: ["models"], "credential-references": ["probe-key"] }],
        tools: [{ name: "hang", description: "test admission", schema: {}, caps: [] }],
      },
    },
    handlers: {
      tools: { hang: (_params, context) => new Promise<{ content: string; data: null; truncated: boolean }>(resolve => {
        context.signal.addEventListener("abort", () => resolve({ content: "settled", data: null, truncated: false }), { once: true })
      }) },
      providers: { "probe/": async function* () { yield { type: "finished", reason: "stop" } } },
      providerModels: { "probe/": async (_params, context) => {
        const response = await context.providerHttp.request("probe-key", httpRequest)
        const body: number[] = []
        for await (const bytes of response.body) body.push(...bytes)
        expect(response.status).toBe(200)
        expect(decoder.decode(new Uint8Array(body))).toBe("catalog")
        return { models: [] }
      } },
    },
  }
}

type Frame = Record<string, JsonValue>
function harness(definition = fixture(), timeout = 500) {
  let input!: ReadableStreamDefaultController<Uint8Array>
  const stream = new ReadableStream<Uint8Array>({ start(controller) { input = controller } })
  const frames: Frame[] = []
  const transport: ServerTransport = {
    input: readableStreamBytes(stream),
    output: { write(bytes) {
      const raw: unknown = JSON.parse(decoder.decode(bytes))
      if (raw === null || typeof raw !== "object" || Array.isArray(raw)) throw new Error("invalid output frame")
      frames.push(raw as Frame)
    } },
  }
  const server = new PluginServer(definition, transport, PROTOCOL_LIMITS.maxLineBytes, timeout)
  const send = (frame: JsonValue) => input.enqueue(encoder.encode(`${JSON.stringify(frame)}\n`))
  const serving = server.serve(transport.input).finally(() => input.close())
  send(initialize)
  return { frames, server, send, serving }
}

async function until(predicate: () => boolean): Promise<void> {
  const deadline = Date.now() + 1000
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error("expected protocol progress did not occur")
    await Bun.sleep(1)
  }
}

function httpEvent(id: JsonValue, event: JsonValue): JsonValue {
  return { jsonrpc: "2.0", method: "provider/http_event", params: { request_id: id, event } }
}

function respond(send: (frame: JsonValue) => void, id: JsonValue): void {
  send(httpEvent(id, { type: "head", status: 200, headers: [["content-type", "text/plain"]] }))
  send(httpEvent(id, { type: "body", data_base64: Buffer.from("catalog").toString("base64") }))
  send(httpEvent(id, { type: "finished" }))
  send({ jsonrpc: "2.0", id, result: null })
}

function httpId(frames: Frame[]): JsonValue {
  const id = frames.find((frame) => frame.method === "provider/http")?.id
  if (id === undefined) throw new Error("missing HTTP request")
  return id
}

describe("production SDK duplex serve", () => {
  test("catalog handler consumes correlated authenticated HTTP while serve keeps reading", async () => {
    const { frames, send, serving } = harness()
    send(models)
    await until(() => frames.some((frame) => frame.method === "provider/http"))
    const request = frames.find((frame) => frame.method === "provider/http")
    expect(request?.params).toMatchObject({ alias: "probe/", credential_reference: "probe-key" })
    respond(send, httpId(frames))
    await until(() => frames.some((frame) => frame.id === 2))
    send(stop)
    await serving
    expect(frames.find((frame) => frame.id === 2)).toEqual({ jsonrpc: "2.0", id: 2, result: { models: [] } })
  })

  test("handler saturation rejects excess requests but still reads HTTP and shutdown", async () => {
    const { frames, send, serving } = harness()
    send(models)
    await until(() => frames.some((frame) => frame.method === "provider/http"))
    for (let id = 10; id < 74; id += 1) send({ jsonrpc: "2.0", id, method: "tool/call", params: { name: "hang", input: {}, lifetime: { total_ms: 300000, idle_ms: 90000 } } })
    await until(() => frames.some((frame) => frame.id === 73))
    expect(frames.find((frame) => frame.id === 73)?.error).toMatchObject({ code: -32005 })
    respond(send, httpId(frames))
    await until(() => frames.some((frame) => frame.id === 2))
    send(stop)
    await serving
    expect(frames.find((frame) => frame.id === 2)?.result).toEqual({ models: [] })
    for (let id = 10; id < 73; id += 1) expect(frames.find((frame) => frame.id === id)?.error).toMatchObject({ code: -32800 })
    expect(frames.find((frame) => frame.id === 999)?.result).toBeNull()
  })

  test("timed-out uncooperative handlers keep occupying admission slots without reporting completion", async () => {
    let invoked = 0
    let cancelled = 0
    const release = Promise.withResolvers<{ content: string; data: null; truncated: boolean }>()
    const definition = fixture()
    const { frames, send, serving } = harness({ ...definition, handlers: {
      ...definition.handlers,
      tools: { hang: (_params, context) => {
        invoked += 1
        context.signal.addEventListener("abort", () => { cancelled += 1 }, { once: true })
        return release.promise
      } },
    } }, 2000)
    try {
      for (let id = 10; id < 74; id += 1) send({ jsonrpc: "2.0", id, method: "tool/call", params: { name: "hang", input: {}, lifetime: { total_ms: 20, idle_ms: 20 } } })
      await until(() => cancelled === 64)
      expect(frames.filter(frame => typeof frame.id === "number" && frame.id >= 10)).toHaveLength(0)
      send({ jsonrpc: "2.0", id: 74, method: "tool/call", params: { name: "hang", input: {}, lifetime: { total_ms: 20, idle_ms: 20 } } })
      await until(() => frames.some(frame => frame.id === 74))
      expect(invoked).toBe(64)
      expect(frames.find(frame => frame.id === 74)?.error).toMatchObject({ code: -32005 })
      release.resolve({ content: "settled", data: null, truncated: false })
      await until(() => frames.filter(frame => typeof frame.id === "number" && frame.id >= 10).length === 65)
      send(stop)
      await serving
    } finally {
      release.resolve({ content: "settled", data: null, truncated: false })
    }
  })

  test("provider cancellation retains ownership through ignored abort and iterator cleanup", async () => {
    const handlerGate = Promise.withResolvers<void>()
    const cleanupGate = Promise.withResolvers<void>()
    let started = false
    let cancelled = false
    let cleaning = false
    let cleaned = false
    let stopped = false
    const definition = fixture()
    const { frames, send, serving } = harness({ ...definition, handlers: { ...definition.handlers,
      providers: { "probe/": async function* (_params, context) {
        context.signal.addEventListener("abort", () => { cancelled = true }, { once: true })
        started = true
        try {
          await handlerGate.promise
          yield { type: "finished", reason: "stop" }
        } finally {
          cleaning = true
          await cleanupGate.promise
          cleaned = true
        }
      } },
    } }, 1000)
    void serving.then(() => { stopped = true })
    send({ jsonrpc: "2.0", id: 2, method: "provider/complete", params: { alias: "probe/model", request: providerRequest } })
    try {
      await until(() => started)
      send(stop)
      await until(() => cancelled)
      await Bun.sleep(20)
      expect(frames.some(frame => frame.id === 2)).toBe(false)
      expect(stopped).toBe(false)
      handlerGate.resolve()
      await until(() => cleaning)
      await Bun.sleep(20)
      expect(frames.some(frame => frame.id === 2)).toBe(false)
      expect(stopped).toBe(false)
      cleanupGate.resolve()
      await serving
      expect(cleaned).toBe(true)
      expect(frames.find(frame => frame.id === 2)?.error).toMatchObject({ code: -32800 })
    } finally {
      handlerGate.resolve()
      cleanupGate.resolve()
      await serving
    }
  })

  test("shutdown settles HTTP requests awaiting the response head", async () => {
    const { frames, send, serving } = harness()
    send(models)
    await until(() => frames.some((frame) => frame.method === "provider/http"))
    send(stop)
    await serving
    expect(frames.find((frame) => frame.id === 2)?.error).toMatchObject({ code: -32800 })
    expect(frames.filter((frame) => frame.method === "provider/http_cancel")).toHaveLength(1)
  })

  test("full unread HTTP bodies cancel that request without blocking control frames", async () => {
    const definition = fixture()
    const { frames, send, serving } = harness({ ...definition, handlers: {
      ...definition.handlers,
      providerModels: { "probe/": async (_params, context) => {
        await context.providerHttp.request("probe-key", httpRequest)
        await new Promise<void>(resolve => context.signal.addEventListener("abort", () => resolve(), { once: true }))
        return { models: [] }
      } },
    } })
    send(models)
    await until(() => frames.some((frame) => frame.method === "provider/http"))
    const id = httpId(frames)
    send(httpEvent(id, { type: "head", status: 200, headers: [] }))
    for (let i = 0; i < 65; i += 1) send(httpEvent(id, { type: "body", data_base64: "YQ==" }))
    await until(() => frames.some((frame) => frame.method === "provider/http_cancel"))
    send(stop)
    await serving
    expect(frames.filter((frame) => frame.method === "provider/http_cancel")).toHaveLength(1)
    expect(frames.find((frame) => frame.id === 999)?.result).toBeNull()
  })

  test("host HTTP permission errors remain correlated and secret-safe", async () => {
    const { frames, send, serving } = harness()
    send(models)
    await until(() => frames.some((frame) => frame.method === "provider/http"))
    send({ jsonrpc: "2.0", id: httpId(frames), error: { code: -32003, message: "secret", data: { code: "permission_denied", secret: "token" } } })
    await until(() => frames.some((frame) => frame.id === 2))
    send(stop)
    await serving
    expect(frames.find((frame) => frame.id === 2)?.error).toEqual({
      code: -32020, message: "host-mediated provider HTTP failed", data: { code: "permission_denied" },
    })
    expect(JSON.stringify(frames)).not.toContain("secret")
  })

  test("tools cannot acquire provider-scoped authenticated HTTP", async () => {
    const definition = fixture()
    const { frames, send, serving } = harness({ ...definition, handlers: {
      ...definition.handlers,
      tools: { hang: async (_params, context) => {
        await context.providerHttp.request("probe-key", httpRequest)
        return { content: "unreachable", data: null, truncated: false }
      } },
    } })
    send({ jsonrpc: "2.0", id: 3, method: "tool/call", params: { name: "hang", input: {}, lifetime: { total_ms: 300000, idle_ms: 90000 } } })
    await until(() => frames.some((frame) => frame.id === 3))
    send(stop)
    await serving
    expect(frames.find((frame) => frame.id === 3)?.error).toMatchObject({ code: -32003 })
    expect(frames.some((frame) => frame.method === "provider/http")).toBe(false)
  })
})

describe("bounded outbound lifecycle", () => {
  test("counts in-flight bytes, fails every pending write on overflow, and sends no later frames", async () => {
    let release!: () => void
    const sent: string[] = []
    const writer = new BoundedJsonWriter({ write(bytes) {
      sent.push(decoder.decode(bytes))
      return new Promise<void>((resolve) => { release = resolve })
    } }, 100, { maxQueuedBytes: 8, maxQueuedFrames: 8 })
    const first = writer.write("a") // Four encoded bytes including newline.
    const second = writer.write("b")
    const pending = Promise.allSettled([first, second, writer.drain()])
    await until(() => sent.length === 1)
    await expect(writer.write("c")).rejects.toBeInstanceOf(OutboundQueueFullError)
    expect((await pending).every((result) => result.status === "rejected")).toBe(true)
    await expect(writer.write("d")).rejects.toBeInstanceOf(OutboundQueueFullError)
    release()
    await Bun.sleep(1)
    expect(sent).toEqual(['"a"\n'])
  })

  test("also bounds frame count and preserves FIFO when the sink makes progress", async () => {
    const sent: string[] = []
    const writer = new BoundedJsonWriter({ write(bytes) { sent.push(decoder.decode(bytes)) } }, 100)
    await Promise.all([writer.write(1), writer.write(2), writer.write(3)])
    await writer.drain()
    expect(sent).toEqual(["1\n", "2\n", "3\n"])
    const blocked = new BoundedJsonWriter({ write: () => new Promise<never>(() => {}) }, 100, { maxQueuedBytes: 100, maxQueuedFrames: 1 })
    const outcome = Promise.allSettled([blocked.write(1)])
    await expect(blocked.write(2)).rejects.toBeInstanceOf(OutboundQueueFullError)
    expect((await outcome)[0]?.status).toBe("rejected")
  })

  test("sink failure settles queued writes without exposing sink errors", async () => {
    const writer = new BoundedJsonWriter({ write() { throw new Error("secret") } })
    const results = await Promise.allSettled([writer.write(1), writer.write(2), writer.drain()])
    expect(results.every((result) => result.status === "rejected" && result.reason.message === "JSON-RPC output write failed")).toBe(true)
  })

  test("external cancellation settles serve even with a stuck output sink", async () => {
    let entered = false
    const input = (async function* () { yield encoder.encode(`${JSON.stringify(initialize)}\n`) })()
    const server = new PluginServer(fixture(), { input, output: { write() { entered = true; return new Promise<never>(() => {}) } } })
    const controller = new AbortController()
    const serving = server.serve(input, PROTOCOL_LIMITS.maxLineBytes, controller.signal)
    await until(() => entered)
    controller.abort()
    await serving
  })

  test("a stuck shutdown response terminates within the configured write deadline", async () => {
    const input = (async function* () {
      yield encoder.encode(`${JSON.stringify(initialize)}\n${JSON.stringify(stop)}\n`)
    })()
    const server = new PluginServer(fixture(), { input, output: { write(bytes) {
      if (decoder.decode(bytes).includes('"id":999')) return new Promise<never>(() => {})
    } } }, PROTOCOL_LIMITS.maxLineBytes, 20)
    await expect(server.serve(input)).rejects.toThrow("JSON-RPC output write timed out")
  })
})

describe("correlated host command outcomes", () => {
  function commandDefinition(): PluginDefinition {
    const base = fixture()
    return {
      manifest: { ...base.manifest, capabilities: {
        ...base.manifest.capabilities, push: ["session/inject_message"],
      } },
      handlers: { ...base.handlers, tools: { hang: async (_params, context) => {
        const result = await context.push.injectMessage("session", "next")
        return { content: result.disposition, data: null, truncated: false }
      } } },
    }
  }

  test("awaits queued injection disposition while the reader services unrelated requests", async () => {
    const { frames, send, serving } = harness(commandDefinition())
    send({ jsonrpc: "2.0", id: 2, method: "tool/call", params: { name: "hang", input: {}, lifetime: { total_ms: 300000, idle_ms: 90000 } } })
    await until(() => frames.some(frame => frame.method === "session/inject_message"))
    expect(frames.some(frame => frame.id === 2)).toBe(false)
    const command = frames.find(frame => frame.method === "session/inject_message")
    if (command?.id === undefined) throw new Error("missing command id")
    send({ jsonrpc: "2.0", id: 3, method: "unknown" })
    await until(() => frames.some(frame => frame.id === 3))
    send({ jsonrpc: "2.0", id: command.id, result: { disposition: "queued" } })
    await until(() => frames.some(frame => frame.id === 2))
    expect(frames.find(frame => frame.id === 2)?.result).toEqual({ content: "queued", data: null, truncated: false })
    send(stop)
    await serving
  })

  test("host rejection reaches the caller", async () => {
    const { frames, send, serving } = harness(commandDefinition())
    send({ jsonrpc: "2.0", id: 2, method: "tool/call", params: { name: "hang", input: {}, lifetime: { total_ms: 300000, idle_ms: 90000 } } })
    await until(() => frames.some(frame => frame.method === "session/inject_message"))
    const command = frames.find(frame => frame.method === "session/inject_message")
    if (command?.id === undefined) throw new Error("missing command id")
    send({ jsonrpc: "2.0", id: command.id, error: { code: -32003, message: "wrong session" } })
    await until(() => frames.some(frame => frame.id === 2))
    expect(frames.find(frame => frame.id === 2)?.error).toMatchObject({ code: -32003 })
    send(stop)
    await serving
  })

  test("malformed responses close transport and settle a pending command handler", async () => {
    for (const malformed of [
      { result: null },
      { id: null, result: null },
      { id: "plugin-push-1", error: null },
      { id: "plugin-push-1", result: null, error: { code: -32603, message: "bad" } },
      { id: "plugin-push-1", result: null, extra: true },
    ]) {
      const { frames, send, serving } = harness(commandDefinition())
      const terminated = serving.then(() => { throw new Error("expected transport rejection") }, (error: unknown) => error)
      send({ jsonrpc: "2.0", id: 2, method: "tool/call", params: { name: "hang", input: {}, lifetime: { total_ms: 300000, idle_ms: 90000 } } })
      await until(() => frames.some(frame => frame.method === "session/inject_message"))
      send({ jsonrpc: "2.0", ...malformed })
      expect(await terminated).toMatchObject({ message: "invalid host response" })
      expect(frames.find(frame => frame.id === 2)?.error).toMatchObject({ code: -32800 })
    }
  })

  test("disconnect rejects a pending host outcome", async () => {
    const { frames, send, serving } = harness(commandDefinition())
    send({ jsonrpc: "2.0", id: 2, method: "tool/call", params: { name: "hang", input: {}, lifetime: { total_ms: 300000, idle_ms: 90000 } } })
    await until(() => frames.some(frame => frame.method === "session/inject_message"))
    send(stop)
    await serving
    expect(frames.find(frame => frame.id === 2)?.error).toMatchObject({ code: -32800 })
  })
})

describe("provider delivery credit", () => {
  test("pauses a burst at the item window while unrelated control remains live", async () => {
    const base = fixture()
    const { frames, send, serving } = harness({ ...base, handlers: { ...base.handlers,
      providers: { "probe/": async function* () {
        for (let n = 0; n < 100; n += 1) yield { type: "text_delta", text: String(n) }
        yield { type: "finished", reason: "stop" }
      } },
    } })
    send({ jsonrpc: "2.0", id: 2, method: "provider/complete", params: { alias: "probe/model", request: providerRequest } })
    send({ jsonrpc: "2.0", method: "provider/credit", params: {
      request_id: 2, events: PROTOCOL_LIMITS.providerWindowEvents, bytes: PROTOCOL_LIMITS.providerWindowBytes,
    } })
    await until(() => frames.filter(frame => frame.method === "provider/event").length === 64)
    send({ jsonrpc: "2.0", id: 3, method: "unknown" })
    await until(() => frames.some(frame => frame.id === 3))
    expect(frames.some(frame => frame.id === 2)).toBe(false)
    const returnedBytes = frames.filter(frame => frame.method === "provider/event").reduce((sum, frame) => {
      const params = frame.params
      if (params === null || typeof params !== "object" || Array.isArray(params)) throw new Error("bad event")
      return sum + encoder.encode(JSON.stringify(frame)).byteLength
    }, 0)
    send({ jsonrpc: "2.0", method: "provider/credit", params: { request_id: 2, events: 64, bytes: returnedBytes } })
    await until(() => frames.some(frame => frame.id === 2))
    expect(frames.filter(frame => frame.method === "provider/event")).toHaveLength(101)
    expect(frames.find(frame => frame.id === 2)?.result).toBeNull()
    send(stop)
    await serving
  })
})

test("shutdown progresses while a provider exhausts its delivery credits", async () => {
  const base = fixture()
  let produced = 0
  let cleaned = false
  const { frames, send, serving } = harness({ ...base, handlers: { ...base.handlers,
    providers: { "probe/": async function* () {
      try {
        for (let n = 0; n < 100; n += 1) {
          produced += 1
          yield { type: "text_delta", text: String(n) }
        }
        yield { type: "finished", reason: "stop" }
      } finally { cleaned = true }
    } },
  } })
  send({ jsonrpc: "2.0", id: 2, method: "provider/complete", params: { alias: "probe/model", request: providerRequest } })
  send({ jsonrpc: "2.0", method: "provider/credit", params: {
    request_id: 2, events: 64, bytes: PROTOCOL_LIMITS.providerWindowBytes,
  } })
  await until(() => produced === 65)
  expect(frames.filter(frame => frame.method === "provider/event")).toHaveLength(64)
  send(stop)
  await serving
  expect(cleaned).toBe(true)
  expect(frames.find(frame => frame.id === 2)?.error).toMatchObject({ code: -32800 })
  expect(frames.find(frame => frame.id === 999)?.result).toBeNull()
})

test("production writer prioritizes control while preserving each provider terminal order", async () => {
  const base = fixture()
  let input!: ReadableStreamDefaultController<Uint8Array>
  const stream = new ReadableStream<Uint8Array>({ start(controller) { input = controller } })
  const frames: Frame[] = []
  let release!: () => void
  let blocked = false
  const held = new Promise<void>(resolve => { release = resolve })
  const transport: ServerTransport = {
    input: readableStreamBytes(stream),
    output: { async write(bytes) {
      const raw: unknown = JSON.parse(decoder.decode(bytes))
      if (raw === null || typeof raw !== "object" || Array.isArray(raw)) throw new Error("invalid output frame")
      const frame = raw as Frame
      if (frame.method === "provider/event" && !blocked) { blocked = true; await held }
      frames.push(frame)
    } },
  }
  const server = new PluginServer({ ...base, handlers: { ...base.handlers,
    providers: { "probe/": async function* () {
      yield { type: "text_delta", text: "first" }
      yield { type: "finished", reason: "stop" }
    } },
  } }, transport, PROTOCOL_LIMITS.maxLineBytes, 1000)
  const send = (frame: JsonValue) => input.enqueue(encoder.encode(`${JSON.stringify(frame)}\n`))
  const serving = server.serve(transport.input).finally(() => input.close())
  send(initialize)
  for (const id of [2, 3]) {
    send({ jsonrpc: "2.0", id, method: "provider/complete", params: { alias: "probe/model", request: providerRequest } })
    send({ jsonrpc: "2.0", method: "provider/credit", params: {
      request_id: id, events: 64, bytes: PROTOCOL_LIMITS.providerWindowBytes,
    } })
  }
  await until(() => blocked)
  send({ jsonrpc: "2.0", id: 4, method: "unknown" })
  await Bun.sleep(10)
  expect(frames.some(frame => frame.id === 2 || frame.id === 3 || frame.id === 4)).toBe(false)
  release()
  await until(() => frames.some(frame => frame.id === 2) && frames.some(frame => frame.id === 3))
  const control = frames.findIndex(frame => frame.id === 4)
  const data = frames.flatMap((frame, index) => frame.method === "provider/event" ? [index] : [])
  expect(control).toBeGreaterThan(data[0] ?? Infinity)
  expect(control).toBeLessThan(data[1] ?? -1)
  for (const id of [2, 3]) {
    const terminal = frames.findIndex(frame => {
      const params = frame.params
      if (frame.method !== "provider/event" || params === null || typeof params !== "object" || Array.isArray(params)) return false
      const event = params.event
      return params.request_id === id && event !== null && typeof event === "object" && !Array.isArray(event) && event.type === "finished"
    })
    expect(frames.findIndex(frame => frame.id === id)).toBeGreaterThan(terminal)
  }
  send(stop)
  await serving
})

test("concurrent command contexts carry their exact host origin through duplex controls", async () => {
  const definition: PluginDefinition = {
    manifest: { name: "control-origin", version: "1", protocol: 3, capabilities: {
      commands: [{ name: "control", description: "control" }], push: ["session/control"],
    } },
    handlers: { commands: { control: async (_params, { session }) => session.control({ action: "select_mode", mode: "plan" }) } },
  }
  const { frames, send, serving } = harness(definition)
  const origins = ["01".repeat(16), "02".repeat(16)]
  for (const [index, origin] of origins.entries()) {
    send({ jsonrpc: "2.0", id: 10 + index, method: "command/execute", params: { name: "control", arguments: "", invocation_id: origin, lifetime:{total_ms:300000,idle_ms:300000} } })
  }
  await until(() => frames.filter(frame => frame.method === "session/control").length === 2)
  const requests = frames.filter(frame => frame.method === "session/control")
  expect(requests.map(frame => frame.params)).toEqual(origins.map(origin => ({ origin, control: { action: "select_mode", mode: "plan" } })))
  for (const request of requests.toReversed()) send({ jsonrpc: "2.0", id: request.id!, result: { outcome: "applied" } })
  await until(() => frames.some(frame => frame.id === 10) && frames.some(frame => frame.id === 11))
  send({ jsonrpc: "2.0", id: 12, method: "command/execute", params: { name: "control", arguments: "", lifetime:{total_ms:300000,idle_ms:300000} } })
  await until(() => frames.some(frame => frame.id === 12))
  expect(frames.find(frame => frame.id === 12)?.error).toMatchObject({ code: -32602 })
  send(stop)
  await serving
})

test("command lifetime owns the actual handler beyond the ordinary control timer", async () => {
  let release!: () => void
  let signal: AbortSignal | undefined
  const definition: PluginDefinition = {
    manifest: { name: "command-lifetime", version: "1", protocol: 3, capabilities: {
      commands: [{ name: "wait", description: "wait for owned effect" }],
    } },
    handlers: { commands: { wait: async (_params, context) => {
      signal = context.signal
      await new Promise<void>(resolve => { release = resolve })
      return { settled: true }
    } } },
  }
  const { frames, send, serving } = harness(definition, 1)
  send({jsonrpc:"2.0",id:10,method:"command/execute",params:{name:"wait",arguments:"",invocation_id:"01".repeat(16),lifetime:{total_ms:150,idle_ms:150}}})
  await until(() => signal !== undefined)
  await new Promise(resolve => setTimeout(resolve, 20))
  expect(signal?.aborted).toBe(false)
  await until(() => signal?.aborted === true)
  expect(frames.some(frame => frame.id === 10)).toBe(false)
  release()
  await until(() => frames.some(frame => frame.id === 10))
  expect(frames.find(frame => frame.id === 10)?.error).toMatchObject({code:-32004})
  send(stop)
  await serving
})
