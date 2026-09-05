import { describe, expect, test } from "bun:test"
import { cpSync, readFileSync, readdirSync } from "node:fs"
import { mkdtemp, readFile, rm, symlink } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"

import {
  BoundedJsonWriter,
  definePlugin,
  LineTooLargeError,
  parsePluginManifest,
  UnterminatedLineError,
  PluginServer,
  PROTOCOL_LIMITS,
  readBoundedLines,
  readableStreamBytes,
  renderTypeScriptScaffold,
  RPC_METHODS,
  scaffoldTypeScriptPlugin,
  type JsonValue,
  type PluginDefinition,
  type RpcOutput,
  type ServerTransport,
} from "../src/index.ts"

const encoder = new TextEncoder()
const decoder = new TextDecoder()
const initializeParams = {
  host: "rottweiler",
  protocol: 3,
  max_frame_bytes: PROTOCOL_LIMITS.maxLineBytes,
} as const

const providerRequest: JsonValue = {
  model: "model",
  turns: [],
  tools: [],
  tool_choice: { mode: "auto" },
  max_output_tokens: 64,
  cache_hint: null,
  temperature: null,
  thinking: "off",
}

function fixtureDefinition(secret = "handler-secret"): PluginDefinition {
  return definePlugin({
    manifest: {
      name: "fixture",
      version: "1.0.0",
      protocol: 3,
      capabilities: {
        tools: [{ name: "echo", description: "echo", schema: { type: "object" }, caps: [] }],
        commands: [{ name: "fixture", description: "fixture command" }],
        hooks: [{ name: "pre_tool", class: "policy", failure_policy: "fail-closed" }],
        providers: [{ "alias-prefix": "fixture/" }],
        event_subscriptions: ["turn_finished"],
        push: ["ui/notify", "session/set_status"],
      },
    },
    handlers: {
      tools: {
        echo: async ({ input }, { push }) => {
          await push.notify("fixture", "called")
          if (input.fail === true) throw new Error(secret)
          return { content: JSON.stringify(input), data: input }
        },
      },
      commands: { fixture: ({ arguments: args }) => ({ arguments: args }) },
      hooks: { pre_tool: () => ({ decision: "block", message: "fixture deny" }) },
      providers: {
        "fixture/": async function* ({ alias }) {
          yield { type: "message_start", model: alias }
          yield { type: "text_delta", text: alias }
          yield { type: "finished", reason: "stop" }
        },
      },
      events: { turn_finished: async ({ cursor }, { push }) => {
        await push.setStatus(cursor.session_id, "done")
        return { mutations: [] }
      } },
    },
  })
}

function harness(definition = fixtureDefinition()): {
  server: PluginServer
  messages: JsonValue[]
  errors: string[]
} {
  const messages: JsonValue[] = []
  const errors: string[] = []
  let server!: PluginServer
  const output: RpcOutput = {
    write(line) {
      const frame = JSON.parse(decoder.decode(line)) as Record<string, JsonValue>
      messages.push(frame)
      if (typeof frame.id === "string" && frame.id.startsWith("plugin-push-")) {
        queueMicrotask(() => { void server.handleLine(JSON.stringify({ jsonrpc: "2.0", id: frame.id, result: null })) })
      }
    },
  }
  const transport: ServerTransport = {
    input: (async function* () {})(),
    output,
    error: { write: (message) => errors.push(message) },
  }
  server = new PluginServer(definition, transport)
  return { server, messages, errors }
}

async function request(server: PluginServer, id: number, method: string, params?: JsonValue): Promise<void> {
  await server.handleLine(JSON.stringify({ jsonrpc: "2.0", id, method, ...(params === undefined ? {} : { params }) }))
  if (method === RPC_METHODS.providerComplete) await server.handleLine(JSON.stringify({
    jsonrpc: "2.0", method: RPC_METHODS.providerCredit,
    params: { request_id: id, events: PROTOCOL_LIMITS.providerWindowEvents, bytes: PROTOCOL_LIMITS.providerWindowBytes },
  }))
}

async function waitFor(predicate: () => boolean, timeoutMs = 2_000): Promise<void> {
  const deadline = Date.now() + timeoutMs
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error("condition timed out")
    await Bun.sleep(1)
  }
}

describe("wire protocol", () => {
  test("rejects incomplete provider requests before invoking a handler", async () => {
    const { server, messages } = harness()
    await request(server, 1, "initialize", initializeParams)
    await request(server, 2, "provider/complete", { alias: "fixture/model", request: {} })
    await waitFor(() => messages.some(message => typeof message === "object" && message !== null && !Array.isArray(message) && message.id === 2))
    expect(messages).toContainEqual({ jsonrpc: "2.0", id: 2, error: { code: -32602, message: "invalid provider request" } })
    expect(messages.some(message => typeof message === "object" && message !== null && !Array.isArray(message) && message.method === "provider/event")).toBe(false)
    await request(server, 3, "shutdown", {})
  })

  test("exports the frozen canonical method table", () => {
    expect(Object.isFrozen(RPC_METHODS)).toBe(true)
    expect(RPC_METHODS).toEqual({
      initialize: "initialize",
      toolCall: "tool/call",
      toolProgress: "tool/progress",
      commandExecute: "command/execute",
      hookInvoke: "hook/invoke",
      providerComplete: "provider/complete",
      providerModels: "provider/models",
      providerEvent: "provider/event",
      providerCredit: "provider/credit",
      providerHttp: "provider/http",
      providerHttpEvent: "provider/http_event",
      providerHttpCancel: "provider/http_cancel",
      eventPublish: "event/publish",
      eventRead: "event/read",
      sessionQuery: "session/query",
      contextRead: "session/context_read",
      sessionControl: "session/control",
      stateRead: "extension/state_read",
      stateCommit: "extension/state_commit",
      injectMessage: "session/inject_message",
      setStatus: "session/set_status",
      notify: "ui/notify",
      publishPanel: "ui/publish_panel",
      shutdown: "shutdown",
      exit: "exit",
    })
  })

  test("the protocol fixture and schema publish the negotiated model catalog contract", async () => {
    const fixture = JSON.parse(
      await readFile(join(import.meta.dir, "../fixtures/wire/protocol-3.json"), "utf8"),
    ) as {
      protocol: number
      methods: Readonly<Record<string, string>>
      provider_models_response: { result: { models: readonly [{ id: string }] } }
    }
    const schema = JSON.parse(
      await readFile(join(import.meta.dir, "../fixtures/wire/protocol-3.schema.json"), "utf8"),
    ) as { properties: { protocol: { const: number } }; $defs: Record<string, unknown> }
    expect(fixture.protocol).toBe(schema.properties.protocol.const)
    expect(fixture.methods).toEqual(RPC_METHODS)
    expect(fixture.methods.providerModels).toBe("provider/models")
    expect(fixture.provider_models_response.result.models[0]?.id).toBe("vision-thinking")
    expect(schema.$defs).toMatchObject({
      provider_declaration: expect.any(Object),
      model_capabilities: expect.any(Object),
      pricing: expect.any(Object),
      provider_models_result: expect.any(Object),
    })
  })

  test("initializes and dispatches every declared request kind", async () => {
    const { server, messages } = harness()
    await request(server, 1, RPC_METHODS.initialize, initializeParams)
    await request(server, 2, RPC_METHODS.toolCall, {
      lifetime: { total_ms: 300000, idle_ms: 90000 }, name: "echo", input: { value: 7 },
    })
    await request(server, 3, RPC_METHODS.commandExecute, {
      name: "fixture", arguments: "hello", invocation_id: null,
    })
    await request(server, 4, RPC_METHODS.hookInvoke, {
      hook: "pre_tool", payload: { id: "call", name: "bash", arguments: {} },
    })
    await request(server, 5, RPC_METHODS.providerComplete, {
      alias: "fixture/model", request: providerRequest,
    })
    await waitFor(() => messages.some((message) =>
      typeof message === "object" && message !== null && "id" in message && message.id === 5
    ))
    await request(server, 6, RPC_METHODS.eventPublish, {
      cursor: { session_id: "s", sequence: "4" }, event: "turn_finished", state_revision: null, content: { storage: "inline", data: { type: "turn_finished" } },
    })
    expect(messages).toHaveLength(11)
    expect(messages[0]).toMatchObject({ id: 1, result: { protocol: 3 } })
    expect(messages[1]).toEqual({
      jsonrpc: "2.0", id: "plugin-push-1", method: "ui/notify", params: { title: "fixture", message: "called" },
    })
    expect(messages[2]).toEqual({
      jsonrpc: "2.0", id: 2, result: { content: '{"value":7}', data: { value: 7 } },
    })
    expect(messages.slice(5, 8)).toEqual([
      { jsonrpc: "2.0", method: "provider/event", params: { request_id: 5, event: { type: "message_start", model: "fixture/model" } } },
      { jsonrpc: "2.0", method: "provider/event", params: { request_id: 5, event: { type: "text_delta", text: "fixture/model" } } },
      { jsonrpc: "2.0", method: "provider/event", params: { request_id: 5, event: { type: "finished", reason: "stop" } } },
    ])
    expect(messages[10]).toEqual({ jsonrpc: "2.0", id: 6, result: { mutations: [] } })
  })

  test("runs the pre_tool policy and custom-tool conformance plugin over stdio", async () => {
    const requests = [
      { jsonrpc: "2.0", id: 1, method: "initialize", params: initializeParams },
      { jsonrpc: "2.0", id: 2, method: "tool/call", params: { lifetime: { total_ms: 300000, idle_ms: 90000 }, name: "fixture_echo", input: { text: "hello" } } },
      { jsonrpc: "2.0", id: 3, method: "hook/invoke", params: { hook: "pre_tool", payload: { id: "call", name: "bash", arguments: {} } } },
      { jsonrpc: "2.0", id: 4, method: "shutdown" },
    ]
    const child = Bun.spawn([process.execPath, join(import.meta.dir, "../fixtures/conformance/pre-tool-deny-custom-tool.ts")], {
      stdin: "pipe", stdout: "pipe", stderr: "pipe", timeout: 5000,
    })
    const reader = readBoundedLines(readableStreamBytes(child.stdout))[Symbol.asyncIterator]()
    const responses: unknown[] = []
    try {
      for (const frame of requests) {
        child.stdin.write(`${JSON.stringify(frame)}\n`)
        await child.stdin.flush()
        const response = await reader.next()
        if (response.done) throw new Error("plugin closed before responding")
        responses.push(JSON.parse(response.value))
      }
      child.stdin.end()
      expect(await child.exited).toBe(0)
      expect(await new Response(child.stderr).text()).toBe("")
      expect(responses[1]).toEqual({ jsonrpc: "2.0", id: 2, result: { content: "hello", data: { text: "hello" } } })
      expect(responses[2]).toEqual({ jsonrpc: "2.0", id: 3, result: { decision: "block", message: "conformance policy denies bash" } })
    } finally {
      child.kill()
      await child.exited
    }
  })

  test("runs event and incrementally streamed provider conformance plugins over stdio", async () => {
    const eventChild = Bun.spawn(["bun", join(import.meta.dir, "../fixtures/conformance/event-subscriber.ts")], { stdin: "pipe", stdout: "pipe", stderr: "pipe" })
    const eventReader = readBoundedLines(readableStreamBytes(eventChild.stdout))[Symbol.asyncIterator]()
    const nextEventResponse = async () => {
      const response = await eventReader.next()
      if (response.done) throw new Error("event plugin ended before response")
      return JSON.parse(response.value)
    }
    try {
      eventChild.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: initializeParams }) + "\n")
      expect(await nextEventResponse()).toMatchObject({ id: 1, result: { protocol: 3 } })
      eventChild.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: 2, method: "event/publish", params: {
        cursor: { session_id: "s", sequence: "4" }, event: "turn_finished", state_revision: null, content: { storage: "inline", data: { type: "turn_finished" } },
      } }) + "\n")
      const push = await nextEventResponse()
      expect(push).toEqual({ jsonrpc: "2.0", id: "plugin-push-1", method: "session/set_status", params: { session_id: "s", status: "turn complete" } })
      eventChild.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: push.id, result: null }) + "\n")
      expect(await nextEventResponse()).toEqual({ jsonrpc: "2.0", id: 2, result: { mutations: [] } })
      eventChild.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: 3, method: "shutdown", params: {} }) + "\n")
      expect(await nextEventResponse()).toMatchObject({ id: 3, result: null })
      eventChild.stdin.end()
      expect(await eventChild.exited).toBe(0)
    } finally { eventChild.kill(); await eventChild.exited }

    const providerWire = [
      { jsonrpc: "2.0", id: 1, method: "initialize", params: initializeParams },
      {
        jsonrpc: "2.0", id: 2, method: "provider/complete",
        params: { alias: "fixture/model", request: providerRequest },
      },
      { jsonrpc: "2.0", method: "provider/credit", params: {
        request_id: 2, events: PROTOCOL_LIMITS.providerWindowEvents, bytes: PROTOCOL_LIMITS.providerWindowBytes,
      } },
    ].map((line) => JSON.stringify(line)).join("\n") + "\n"
    const providerChild = Bun.spawn(
      ["bun", join(import.meta.dir, "../fixtures/conformance/provider.ts")],
      { stdin: "pipe", stdout: "pipe", stderr: "pipe" },
    )
    providerChild.stdin.write(providerWire)
    providerChild.stdin.flush()
    const providerResponses: JsonValue[] = []
    let firstDeltaAt: number | undefined
    let completedAt: number | undefined
    for await (const line of readBoundedLines(readableStreamBytes(providerChild.stdout))) {
      const response = JSON.parse(line) as JsonValue
      providerResponses.push(response)
      if (typeof response === "object" && response !== null && !Array.isArray(response)) {
        if (response.method === "provider/event") {
          const params = response.params as { event?: { type?: string } } | undefined
          if (params?.event?.type === "text_delta") firstDeltaAt = performance.now()
        }
        if (response.id === 2) {
          completedAt = performance.now()
          providerChild.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id: 3, method: "shutdown", params: {} })}\n`)
          providerChild.stdin.end()
        }
        if (response.id === 3) break
      }
    }
    expect(await providerChild.exited).toBe(0)
    expect(firstDeltaAt).toBeNumber()
    expect(completedAt).toBeNumber()
    expect((completedAt ?? 0) - (firstDeltaAt ?? 0)).toBeGreaterThanOrEqual(50)
    expect(providerResponses.filter((response) =>
      typeof response === "object" && response !== null && !Array.isArray(response) && response.method === "provider/event"
    )).toHaveLength(4)
    expect(providerResponses.some((response) =>
      typeof response === "object" && response !== null && !Array.isArray(response)
        && response.id === 2 && response.result === null
    )).toBe(true)
  })

  test("shutdown aborts and cleans a cooperative provider", async () => {
    let cleaned = false
    const definition = definePlugin({
      manifest: {
        name: "cancel-provider", version: "1", protocol: 3,
        capabilities: { providers: [{ "alias-prefix": "fixture/" }] },
      },
      handlers: { providers: {
        "fixture/": async function* (_params, { signal }) {
          try {
            yield { type: "text_delta", text: "first" }
            await new Promise<void>((resolve) => signal.addEventListener("abort", () => resolve(), { once: true }))
          } finally {
            cleaned = true
          }
        },
      } },
    })
    const { server, messages } = harness(definition)
    await request(server, 1, RPC_METHODS.initialize, initializeParams)
    await request(server, 2, RPC_METHODS.providerComplete, {
      alias: "fixture/model", request: providerRequest,
    })
    await waitFor(() => messages.some((message) =>
      typeof message === "object" && message !== null && !Array.isArray(message)
        && message.method === RPC_METHODS.providerEvent
    ))
    await request(server, 3, RPC_METHODS.shutdown, {})
    await waitFor(() => cleaned && messages.some((message) =>
      typeof message === "object" && message !== null && !Array.isArray(message)
        && message.id === 2 && "error" in message
    ))
    expect(cleaned).toBe(true)
    expect(messages.filter((message) =>
      typeof message === "object" && message !== null && !Array.isArray(message)
        && message.method === RPC_METHODS.providerEvent
    )).toHaveLength(1)
  })

  test("exercises the raw capability-violator host fixture", () => {
    const wire = [
      { jsonrpc: "2.0", id: 1, method: "initialize", params: initializeParams },
      { jsonrpc: "2.0", id: 2, method: "tool/call", params: { lifetime: { total_ms: 300000, idle_ms: 90000 }, name: "escaped", input: {} } },
    ].map((line) => JSON.stringify(line)).join("\n") + "\n"
    const child = Bun.spawnSync(
      ["bun", join(import.meta.dir, "../fixtures/conformance/capability-violator.ts")],
      { stdin: encoder.encode(wire), stdout: "pipe", stderr: "pipe", timeout: 5_000 },
    )
    expect(child.exitCode).toBe(0)
    const responses = child.stdout.toString().trim().split("\n").map((line) => JSON.parse(line) as JsonValue)
    expect(responses[0]).toMatchObject({ result: { capabilities: {} } })
    expect(responses[1]).toMatchObject({ result: { escaped: true } })
  })

  test("never serializes handler exceptions or secret-bearing stacks", async () => {
    const secret = "CANARY_DO_NOT_LOG"
    const { server, messages, errors } = harness(fixtureDefinition(secret))
    await request(server, 1, RPC_METHODS.initialize, initializeParams)
    await request(server, 2, RPC_METHODS.toolCall, {
      lifetime: { total_ms: 300000, idle_ms: 90000 }, name: "echo", input: { fail: true },
    })
    expect(JSON.stringify(messages)).not.toContain(secret)
    expect(errors.join("")).not.toContain(secret)
    expect(messages.at(-1)).toEqual({
      jsonrpc: "2.0", id: 2, error: { code: -32603, message: "plugin tool failed" },
    })
  })

  test("rejects calls before initialize and a mismatched protocol", async () => {
    const { server, messages } = harness()
    await request(server, 1, RPC_METHODS.toolCall, {})
    await request(server, 2, RPC_METHODS.initialize, { ...initializeParams, protocol: 4 })
    expect(messages).toEqual([
      { jsonrpc: "2.0", id: 1, error: { code: -32002, message: "plugin is not initialized" } },
      { jsonrpc: "2.0", id: 2, error: { code: -32001, message: "unsupported plugin protocol" } },
    ])
  })

  test("rejects a range field in initialize", async () => {
    const { server, messages } = harness()
    await request(server, 1, RPC_METHODS.initialize, { ...initializeParams, min_protocol: 3 })
    expect(messages.at(-1)).toMatchObject({ id: 1, error: { code: -32602 } })
  })

  test("initializes protocol 3 and publishes a bounded provider catalog", async () => {
    const definition = definePlugin({
      manifest: {
        name: "catalog-provider", version: "1", protocol: 3,
        capabilities: {
          providers: [{ "alias-prefix": "catalog/", capabilities: ["models"] }],
        },
      },
      handlers: {
        providers: {
          "catalog/": async function* () { yield { type: "finished", reason: "stop" } },
        },
        providerModels: {
          "catalog/": () => ({ models: [{
            id: "capable", display_name: "Capable",
            capabilities: {
              tool_calling: true, vision: true, thinking: true, cache_breakpoints: "explicit",
            },
            max_context_tokens: 200_000, max_output_tokens: 16_000,
            pricing: {
              input_per_million_micros_usd: 3_000_000,
              output_per_million_micros_usd: 15_000_000,
            },
          }] }),
        },
      },
    })
    const { server, messages } = harness(definition)
    await request(server, 1, RPC_METHODS.initialize, {
      ...initializeParams, protocol: 3, capabilities: ["provider-models", "future-host-capability"],
    })
    await request(server, 2, RPC_METHODS.providerModels, { alias_prefix: "catalog/" })
    expect(messages[0]).toMatchObject({ id: 1, result: { protocol: 3 } })
    expect(messages[1]).toMatchObject({
      id: 2,
      result: { models: [{ id: "capable", capabilities: { vision: true }, max_context_tokens: 200_000 }] },
    })
  })

  test("protocol 3 refuses initialization without its negotiated catalog capability", async () => {
    const definition = definePlugin({
      manifest: {
        name: "catalog-provider", version: "1", protocol: 3,
        capabilities: { providers: [{ "alias-prefix": "catalog/", capabilities: ["models"] }] },
      },
      handlers: {
        providers: { "catalog/": async function* () { yield { type: "finished", reason: "stop" } } },
        providerModels: { "catalog/": () => ({ models: [] }) },
      },
    })
    const { server, messages } = harness(definition)
    await request(server, 1, RPC_METHODS.initialize, { ...initializeParams, protocol: 3 })
    expect(messages).toEqual([
      { jsonrpc: "2.0", id: 1, error: { code: -32001, message: "unsupported plugin protocol" } },
    ])
  })

  test("parse and unknown-method errors are JSON-RPC compliant", async () => {
    const { server, messages } = harness()
    await server.handleLine("{")
    await request(server, 1, RPC_METHODS.initialize, initializeParams)
    await request(server, 2, "no/such/method", {})
    expect(messages[0]).toEqual({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "parse error" } })
    expect(messages[2]).toEqual({ jsonrpc: "2.0", id: 2, error: { code: -32601, message: "method not found" } })
  })

  test("gracefully shuts down when aborted while input is idle", async () => {
    let shutdowns = 0
    const definition = definePlugin({
      manifest: { name: "shutdown", version: "1", protocol: 3, capabilities: {} },
      handlers: { shutdown: () => { shutdowns += 1 } },
    })
    const { server } = harness(definition)
    const controller = new AbortController()
    const idle = (async function* () { await new Promise<never>(() => undefined) })()
    const serving = server.serve(idle, 1024, controller.signal)
    controller.abort()
    await serving
    expect(shutdowns).toBe(1)
  })

  test("tool cancellation retains ownership until an uncooperative handler settles", async () => {
    let observedAbort = false
    const completion = Promise.withResolvers<{ content: string; data: null }>()
    const definition = definePlugin({
      manifest: { name: "retained-tool", version: "1", protocol: 3, capabilities: {
        tools: [{ name: "hang", description: "hang", schema: {}, caps: [] }],
      } },
      handlers: { tools: { hang: (_params, { signal }) => {
        signal.addEventListener("abort", () => { observedAbort = true }, { once: true })
        return completion.promise
      } } },
    })
    const { server, messages } = harness(definition)
    await request(server, 1, RPC_METHODS.initialize, initializeParams)
    let finished = false
    const pending = request(server, 2, RPC_METHODS.toolCall, {
      lifetime: { total_ms: 20, idle_ms: 20 }, name: "hang", input: {},
    }).then(() => { finished = true })
    await waitFor(() => observedAbort)
    expect(finished).toBe(false)
    expect(messages).toHaveLength(1)
    completion.resolve({ content: "settled", data: null })
    await pending
    expect(messages.at(-1)).toEqual({ jsonrpc: "2.0", id: 2,
      error: { code: -32004, message: "plugin tool deadline exceeded" },
    })
  })

  test("hook timeout retains its handler until cleanup returns", async () => {
    let observedAbort = false
    const completion = Promise.withResolvers<void>()
    const messages: unknown[] = []
    const server = new PluginServer(definePlugin({
      manifest: { name: "retained-hook", version: "1", protocol: 3, capabilities: {
        hooks: [{ name: "pre_tool", class: "observer", failure_policy: "fail-open" }],
      } },
      handlers: { hooks: { pre_tool: async (_input, { signal }) => {
        signal.addEventListener("abort", () => { observedAbort = true }, { once: true })
        await completion.promise
        return { decision: "continue" }
      } } },
    }), { input: (async function* () {})(), output: { write: bytes => { messages.push(JSON.parse(decoder.decode(bytes))) } } }, 4096, 20)
    await request(server, 1, RPC_METHODS.initialize, initializeParams)
    let finished = false
    const pending = request(server, 2, RPC_METHODS.hookInvoke, { hook: "pre_tool", payload: { id: "call", name: "bash", arguments: {} } }).then(() => { finished = true })
    await waitFor(() => observedAbort)
    expect(finished).toBe(false)
    expect(messages).toHaveLength(1)
    completion.resolve()
    await pending
    expect(messages.at(-1)).toEqual({ jsonrpc: "2.0", id: 2, error: { code: -32004, message: "plugin handler timed out" } })
  })

  test("cancels an in-flight handler during shutdown", async () => {
    let observedAbort = false
    const definition = definePlugin({
      manifest: {
        name: "abort-handler", version: "1", protocol: 3,
        capabilities: { tools: [{ name: "hang", description: "hang", schema: {}, caps: [] }] },
      },
      handlers: { tools: { hang: (_params, { signal }) => new Promise<{ content: string; data: null }>(resolve => {
        signal.addEventListener("abort", () => { observedAbort = true; resolve({ content: "settled", data: null }) }, { once: true })
      }) } },
    })
    const { server, messages } = harness(definition)
    await request(server, 1, RPC_METHODS.initialize, initializeParams)
    const pending = request(server, 2, RPC_METHODS.toolCall, { lifetime: { total_ms: 300000, idle_ms: 90000 }, name: "hang", input: {} })
    await Bun.sleep(1)
    await server.shutdown()
    await pending
    expect(observedAbort).toBe(true)
    expect(messages.at(-1)).toEqual({
      jsonrpc: "2.0", id: 2, error: { code: -32800, message: "plugin tool cancelled" },
    })
  })

  test("refuses undeclared pushes locally without emitting a push frame", async () => {
    const definition = definePlugin({
      manifest: {
        name: "no-push", version: "1", protocol: 3,
        capabilities: { tools: [{ name: "attempt", description: "attempt", schema: {}, caps: [] }] },
      },
      handlers: { tools: { attempt: async (_params, { push }) => {
        await push.notify("x", "y")
        return { content: "unreachable", data: null }
      } } },
    })
    const { server, messages } = harness(definition)
    await request(server, 1, RPC_METHODS.initialize, initializeParams)
    await request(server, 2, RPC_METHODS.toolCall, { lifetime: { total_ms: 300000, idle_ms: 90000 }, name: "attempt", input: {} })
    expect(messages).toHaveLength(2)
    expect(messages[1]).toEqual({
      jsonrpc: "2.0", id: 2, error: { code: -32003, message: "push method is not declared" },
    })
  })
})

describe("bounded transport and manifests", () => {
  test("reads fragmented UTF-8 newline frames", async () => {
    async function* chunks() {
      yield encoder.encode('{"text":"')
      yield encoder.encode('🐕"}\r\n{"ok":true}\n')
    }
    const lines: string[] = []
    for await (const line of readBoundedLines(chunks(), 64)) lines.push(line)
    expect(lines).toEqual(['{"text":"🐕"}', '{"ok":true}'])
  })

  test("rejects oversized input and output", async () => {
    async function* chunks() { yield encoder.encode("12345") }
    await expect(async () => {
      for await (const _line of readBoundedLines(chunks(), 4)) void _line
    }).toThrow(LineTooLargeError)
    const writer = new BoundedJsonWriter({ write() {} }, 8)
    await expect(writer.write({ too: "large" })).rejects.toBeInstanceOf(LineTooLargeError)
  })

  test("rejects an unterminated final JSON-RPC line", async () => {
    async function* chunks() { yield encoder.encode('{"jsonrpc":"2.0"}') }
    await expect(async () => {
      for await (const _line of readBoundedLines(chunks(), 64)) void _line
    }).toThrow(UnterminatedLineError)
  })

  test("rejects undeclared handlers and unbounded manifests before startup", () => {
    expect(() => definePlugin({
      manifest: { name: "bad", version: "1", protocol: 3, capabilities: {} },
      handlers: { tools: { escaped: () => ({ content: "escaped", data: null }) } },
    })).toThrow("exceeds the manifest")
    expect(() => definePlugin({
      manifest: {
        name: "x".repeat(PROTOCOL_LIMITS.maxNameBytes + 1),
        version: "1", protocol: 3, capabilities: {},
      },
      handlers: {},
    })).toThrow("plugin name")
  })

  test("matches Rust canonical manifest limits and names", () => {
    expect(() => definePlugin({
      manifest: { name: "version", version: "x".repeat(65), protocol: 3, capabilities: {} },
      handlers: {},
    })).toThrow("plugin version")
    expect(() => definePlugin({
      manifest: {
        name: "event", version: "1", protocol: 3,
        capabilities: { event_subscriptions: ["turnFinished" as never] },
      },
      handlers: {},
    })).toThrow("unknown event subscription")
    expect(() => definePlugin({
      manifest: {
        name: "provider", version: "1", protocol: 3,
        capabilities: { providers: [{ "alias-prefix": "fixture" }] },
      },
      handlers: { providers: { fixture: async function* () { yield { type: "finished", reason: "stop" } } } },
    })).toThrow("ending in /")

    const maximumPrefix = `${"a".repeat(PROTOCOL_LIMITS.maxNameBytes - 1)}/`
    expect(() => definePlugin({
      manifest: {
        name: "provider", version: "1", protocol: 3,
        capabilities: { providers: [{ "alias-prefix": maximumPrefix }] },
      },
      handlers: { providers: { [maximumPrefix]: async function* () { yield { type: "finished", reason: "stop" } } } },
    })).not.toThrow()
    const overlongPrefix = `${"a".repeat(PROTOCOL_LIMITS.maxNameBytes)}/`
    expect(() => definePlugin({
      manifest: {
        name: "provider", version: "1", protocol: 3,
        capabilities: { providers: [{ "alias-prefix": overlongPrefix }] },
      },
      handlers: { providers: { [overlongPrefix]: async function* () { yield { type: "finished", reason: "stop" } } } },
    })).toThrow("ending in /")

    let schema: JsonValue = {}
    for (let depth = 0; depth < 33; depth += 1) schema = { nested: schema }
    expect(() => definePlugin({
      manifest: {
        name: "schema", version: "1", protocol: 3,
        capabilities: {
          tools: [{ name: "deep", description: "deep", schema: schema as never, caps: [] }],
        },
      },
      handlers: { tools: { deep: () => ({ content: "deep", data: null }) } },
    })).toThrow("size or depth")
  })

  test("locks the approved manifest and handler registry", () => {
    const definition = fixtureDefinition()
    expect(Object.isFrozen(definition)).toBe(true)
    expect(Object.isFrozen(definition.manifest.capabilities)).toBe(true)
    expect(Object.isFrozen(definition.handlers.tools)).toBe(true)
  })
})

describe("scaffold", () => {
  test("parses and freezes an inert manifest document", () => {
    const manifest = parsePluginManifest({
      name: "inert",
      version: "1.0.0",
      protocol: 3,
      capabilities: {},
    })
    expect(manifest.name).toBe("inert")
    expect(Object.isFrozen(manifest)).toBe(true)
    expect(() => parsePluginManifest({
      name: "inert",
      version: "1.0.0",
      protocol: 3,
      capabilities: { unknown: [] },
    })).toThrow("unknown field")
    expect(() => parsePluginManifest({
      name: "inert",
      version: "1.0.0",
      protocol: 3,
      capabilities: { hooks: ["pre_tool"] },
    })).toThrow()
  })

  test("is deterministic and contains the conformance hook and custom tool", () => {
    const first = renderTypeScriptScaffold({ name: "Policy Plugin" })
    expect(first).toEqual(renderTypeScriptScaffold({ name: "Policy Plugin" }))
    const source = first.find((file) => file.path === "src/index.ts")?.contents ?? ""
    const manifest = first.find((file) => file.path === "manifest.json")?.contents ?? ""
    expect(manifest).toContain('"name": "hello"')
    expect(manifest).toContain('"failure_policy": "fail-closed"')
    expect(first.some((file) => file.path === "manifest.json")).toBe(true)
    expect(source).toContain("parsePluginManifest(manifestDocument)")
  })

  test("writes once by default and requires force to replace", async () => {
    const directory = await mkdtemp(join(tmpdir(), "rottweiler-sdk-scaffold-"))
    try {
      await scaffoldTypeScriptPlugin(directory, { name: "fixture" })
      expect(await readFile(join(directory, "manifest.json"), "utf8")).toContain('"name": "fixture"')
      await expect(scaffoldTypeScriptPlugin(directory, { name: "fixture" })).rejects.toMatchObject({ code: "EEXIST" })
      await scaffoldTypeScriptPlugin(directory, { name: "fixture", force: true })
    } finally {
      await rm(directory, { recursive: true, force: true })
    }
  })

  test("preflights every target and refuses symlink replacement", async () => {
    const directory = await mkdtemp(join(tmpdir(), "rottweiler-sdk-scaffold-link-"))
    try {
      await symlink(join(directory, "elsewhere"), join(directory, "package.json"))
      await expect(scaffoldTypeScriptPlugin(directory, { force: true })).rejects.toThrow("symlink")
      expect(await readFile(join(directory, "package.json"), "utf8").catch(() => "missing")).toBe("missing")
    } finally {
      await rm(directory, { recursive: true, force: true })
    }
  })

  test("failed builds clean their system-temp staging directory", () => {
    const prefix = "rottweiler-plugin-sdk-build-"
    const before = new Set(readdirSync(tmpdir()).filter((entry) => entry.startsWith(prefix)))
    const failed = Bun.spawnSync(["bun", "run", "build.ts"], {
      cwd: join(import.meta.dir, ".."),
      env: { ...process.env, ROTTWEILER_SDK_TEST_FAIL_AFTER_STAGE: "1" },
      stdout: "ignore",
      stderr: "ignore",
    })
    expect(failed.exitCode).not.toBe(0)
    const additions = readdirSync(tmpdir()).filter((entry) => entry.startsWith(prefix) && !before.has(entry))
    expect(additions).toEqual([])
  })

  test("builds byte-identically from two checkout roots", async () => {
    const roots = await Promise.all([
      mkdtemp(join(tmpdir(), "rottweiler-sdk-repro-a-")),
      mkdtemp(join(tmpdir(), "rottweiler-sdk-repro-b-")),
    ])
    const packageRoot = join(import.meta.dir, "..")
    const copyInputs = ["build.ts", "package.json", "tsconfig.json", "tsconfig.build.json", "src"]
    try {
      for (const root of roots) {
        for (const input of copyInputs) cpSync(join(packageRoot, input), join(root, input), { recursive: true })
        await symlink(join(packageRoot, "node_modules"), join(root, "node_modules"), "dir")
        const built = Bun.spawnSync(["bun", "run", "build.ts"], { cwd: root, stdout: "ignore", stderr: "pipe" })
        expect(built.exitCode, built.stderr.toString()).toBe(0)
      }
      const snapshot = (root: string): Record<string, string> => {
        const result: Record<string, string> = {}
        const walk = (directory: string, prefix = "") => {
          for (const entry of readdirSync(directory, { withFileTypes: true })) {
            const relative = join(prefix, entry.name)
            const absolute = join(directory, entry.name)
            if (entry.isDirectory()) walk(absolute, relative)
            else result[relative] = new Bun.CryptoHasher("sha256").update(readFileSync(absolute)).digest("hex")
          }
        }
        walk(join(root, "dist"))
        return result
      }
      expect(snapshot(roots[0]!)).toEqual(snapshot(roots[1]!))
    } finally {
      await Promise.all(roots.map((root) => rm(root, { recursive: true, force: true })))
    }
  })
})
