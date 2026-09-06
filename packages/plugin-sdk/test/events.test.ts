import { expect, test } from "bun:test"
import { definePlugin, PluginServer, RPC_METHODS, PROTOCOL_LIMITS, type JsonValue, type EventHandler } from "../src/index"
import { eventSourceReader } from "../src/host-events"

const notice = { cursor: { session_id: "s", sequence: "4" }, event: "turn_finished", state_revision: null, content: { storage: "inline", data: { type: "turn_finished", text: "[REDACTED]" } } } as const
function setup(handler: EventHandler) {
  const messages: Record<string, unknown>[] = []
  const definition = definePlugin({ manifest: { name: "events", version: "1", protocol: 3, capabilities: { event_subscriptions: ["turn_finished"] } }, handlers: { events: { turn_finished: handler } } })
  const server = new PluginServer(definition, { input: (async function* () {})(), output: { async write(bytes) { messages.push(JSON.parse(new TextDecoder().decode(bytes))) } } })
  return { server, messages }
}
async function initialize(server: PluginServer) {
  await server.handleLine(JSON.stringify({ jsonrpc: "2.0", id: 1, method: RPC_METHODS.initialize, params: { host: "rottweiler", protocol: 3, max_frame_bytes: PROTOCOL_LIMITS.maxLineBytes } }))
}

test("event delivery requires exact typed envelope and a correlated request", async () => {
  let calls = 0
  const { server, messages } = setup(() => { calls++; return { mutations: [] } })
  await initialize(server)
  await server.handleLine(JSON.stringify({ jsonrpc: "2.0", method: RPC_METHODS.eventPublish, params: notice }))
  expect(calls).toBe(0)
  for (const params of [{ event: "TurnFinished", payload: {} }, { ...notice, state_revision: undefined }, { ...notice, event: "extension_state_committed" }]) {
    await server.handleLine(JSON.stringify({ jsonrpc: "2.0", id: 2, method: RPC_METHODS.eventPublish, params }))
    expect(messages.at(-1)).toMatchObject({ id: 2, error: { code: -32602 } })
  }
  expect(calls).toBe(0)
  await server.handleLine(JSON.stringify({ jsonrpc: "2.0", id: 3, method: RPC_METHODS.eventPublish, params: notice }))
  expect(messages.at(-1)).toEqual({ jsonrpc: "2.0", id: 3, result: { mutations: [] } })
  expect(calls).toBe(1)
})

test("event outcome is emitted only when the actual callback completes", async () => {
  let release!: () => void
  let entered!: () => void
  const began = new Promise<void>(resolve => { entered = resolve })
  const blocked = new Promise<void>(resolve => { release = resolve })
  const { server, messages } = setup(async () => { entered(); await blocked; return { mutations: [{ action: "set", key: "counter", value: 1 }] } })
  await initialize(server)
  const invocation = server.handleLine(JSON.stringify({ jsonrpc: "2.0", id: 2, method: RPC_METHODS.eventPublish, params: notice }))
  await began
  expect(messages.some(message => message.id === 2)).toBe(false)
  release(); await invocation
  expect(messages.at(-1)).toMatchObject({ id: 2, result: { mutations: [{ action: "set", key: "counter", value: 1 }] } })
})

test("source reader binds exact cursor, extent and bounded canonical base64", async () => {
  const sourceNotice = { ...notice, content: { storage: "source", bytes: 3 } } as const
  const response = { cursor: notice.cursor, offset: 0, data_base64: "YWJj", next_offset: null }
  const requests: JsonValue[] = []
  const read = eventSourceReader(sourceNotice, async (method, params) => { expect(method).toBe(RPC_METHODS.eventRead); requests.push(params); return response })
  expect(await read(0, 3)).toEqual(response)
  expect(requests).toEqual([{ cursor: notice.cursor, offset: 0, max_bytes: 3 }])
  await expect(read(0, 65_537)).rejects.toThrow("invalid event source read")
  const foreign = eventSourceReader(sourceNotice, async () => ({ ...response, cursor: { ...notice.cursor, sequence: "5" } }))
  await expect(foreign(0, 3)).rejects.toThrow("invalid event source response")
  const wrongExtent = eventSourceReader(sourceNotice, async () => ({ ...response, next_offset: 3 }))
  await expect(wrongExtent(0, 3)).rejects.toThrow("extent")
})
