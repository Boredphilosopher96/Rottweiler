import { describe, expect, test } from "bun:test"
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

const encoder = new TextEncoder()
const decoder = new TextDecoder()
const initialize = {
  jsonrpc: "2.0", id: 1, method: "initialize",
  params: {
    host: "rottweiler", protocol: 2, min_protocol: 2, max_frame_bytes: PROTOCOL_LIMITS.maxLineBytes,
    capabilities: ["provider-models", "provider-http"],
  },
} satisfies JsonValue
const models = { jsonrpc: "2.0", id: 2, method: "provider/models", params: { alias_prefix: "probe/" } } satisfies JsonValue
const stop = { jsonrpc: "2.0", id: 999, method: "shutdown" } satisfies JsonValue
const httpRequest = { method: "GET", url: "https://example.com/models", credential_header: "authorization" } as const

function fixture(): PluginDefinition {
  return {
    manifest: {
      name: "duplex", version: "1", protocol: 2,
      capabilities: {
        providers: [{ "alias-prefix": "probe/", capabilities: ["models"], "credential-references": ["probe-key"] }],
        tools: [{ name: "hang", description: "test admission", schema: {}, caps: [] }],
      },
    },
    handlers: {
      tools: { hang: () => new Promise<never>(() => {}) },
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
    for (let id = 10; id < 74; id += 1) send({ jsonrpc: "2.0", id, method: "tool/call", params: { name: "hang", input: {} } })
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

  test("timed-out uncooperative handlers keep occupying admission slots", async () => {
    let invoked = 0
    const definition = fixture()
    const { frames, send, serving } = harness({ ...definition, handlers: {
      ...definition.handlers,
      tools: { hang: () => { invoked += 1; return new Promise<never>(() => {}) } },
    } }, 20)
    for (let id = 10; id < 74; id += 1) send({ jsonrpc: "2.0", id, method: "tool/call", params: { name: "hang", input: {} } })
    await until(() => frames.filter((frame) => typeof frame.id === "number" && frame.id >= 10).length === 64)
    send({ jsonrpc: "2.0", id: 74, method: "tool/call", params: { name: "hang", input: {} } })
    await until(() => frames.some((frame) => frame.id === 74))
    send(stop)
    await serving
    expect(invoked).toBe(64)
    expect(frames.find((frame) => frame.id === 74)?.error).toMatchObject({ code: -32005 })
  })

  test("cancel settles a provider even when its iterator ignores abort", async () => {
    let entered = false
    const definition = fixture()
    const { frames, send, serving } = harness({ ...definition, handlers: {
      ...definition.handlers,
      providers: { "probe/": async function* () { entered = true; await new Promise<never>(() => {}) } },
    } })
    send({ jsonrpc: "2.0", id: 3, method: "provider/complete", params: { alias: "probe/model", request: {} } })
    await until(() => entered)
    send({ jsonrpc: "2.0", method: "provider/cancel", params: { request_id: 3 } })
    await until(() => frames.some((frame) => frame.id === 3))
    send(stop)
    await serving
    expect(frames.find((frame) => frame.id === 3)?.error).toMatchObject({ code: -32800 })
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
        return new Promise<never>(() => {})
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
        return { content: "unreachable", data: null }
      } },
    } })
    send({ jsonrpc: "2.0", id: 3, method: "tool/call", params: { name: "hang", input: {} } })
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
        return { content: result.disposition, data: null }
      } } },
    }
  }

  test("awaits queued injection disposition while the reader services unrelated requests", async () => {
    const { frames, send, serving } = harness(commandDefinition())
    send({ jsonrpc: "2.0", id: 2, method: "tool/call", params: { name: "hang", input: {} } })
    await until(() => frames.some(frame => frame.method === "session/inject_message"))
    expect(frames.some(frame => frame.id === 2)).toBe(false)
    const command = frames.find(frame => frame.method === "session/inject_message")
    if (command?.id === undefined) throw new Error("missing command id")
    send({ jsonrpc: "2.0", id: 3, method: "unknown" })
    await until(() => frames.some(frame => frame.id === 3))
    send({ jsonrpc: "2.0", id: command.id, result: { disposition: "queued" } })
    await until(() => frames.some(frame => frame.id === 2))
    expect(frames.find(frame => frame.id === 2)?.result).toEqual({ content: "queued", data: null })
    send(stop)
    await serving
  })

  test("host rejection reaches the caller and malformed outcomes cannot strand a promise", async () => {
    for (const outcome of [{ error: { code: -32003, message: "wrong session" } }, { error: null }]) {
      const { frames, send, serving } = harness(commandDefinition())
      send({ jsonrpc: "2.0", id: 2, method: "tool/call", params: { name: "hang", input: {} } })
      await until(() => frames.some(frame => frame.method === "session/inject_message"))
      const command = frames.find(frame => frame.method === "session/inject_message")
      if (command?.id === undefined) throw new Error("missing command id")
      send({ jsonrpc: "2.0", id: command.id, ...outcome })
      await until(() => frames.some(frame => frame.id === 2))
      expect(frames.find(frame => frame.id === 2)?.error).toMatchObject({
        code: outcome.error === null ? -32603 : -32003,
      })
      send(stop)
      await serving
    }
  })

  test("disconnect rejects a pending host outcome", async () => {
    const { frames, send, serving } = harness(commandDefinition())
    send({ jsonrpc: "2.0", id: 2, method: "tool/call", params: { name: "hang", input: {} } })
    await until(() => frames.some(frame => frame.method === "session/inject_message"))
    send(stop)
    await serving
    expect(frames.find(frame => frame.id === 2)?.error).toMatchObject({ code: -32800 })
  })
})
