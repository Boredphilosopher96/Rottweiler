import { expect, test } from "bun:test"
import { definePlugin, PluginServer, PROTOCOL_LIMITS, type ToolHandler } from "../src/index"
import validateToolResponse from "../src/generated/tool-response-validator.js"

test("tool responses require the source-owned complete wire contract", async () => {
  const complete = { content: "done", data: { count: 1 }, truncated: false }
  const malformed: unknown[] = [
    { content: "done", data: null }, { content: "done", truncated: false },
    { data: null, truncated: false }, { ...complete, truncated: null },
    { ...complete, presentation: null }, { ...complete, protected_framing: null },
    { ...complete, unknown: true },
  ]
  expect(validateToolResponse(complete)).toBe(true)
  const frames: Record<string, unknown>[] = []
  let returned: unknown = complete
  const handler = (() => returned) as ToolHandler
  const server = new PluginServer(definePlugin({
    manifest: { name: "tool-result", version: "1", protocol: 3, capabilities: {
      tools: [{ name: "result", description: "Result contract", schema: {}, caps: [] }],
    } }, handlers: { tools: { result: handler } },
  }), {
    input: (async function* () {})(),
    output: { write(bytes) { frames.push(JSON.parse(new TextDecoder().decode(bytes))) } },
  })
  await server.handleLine(JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: {
    host: "rottweiler", protocol: 3, max_frame_bytes: PROTOCOL_LIMITS.maxLineBytes,
  } }))
  let id = 2
  for (const value of [complete, ...malformed]) {
    returned = value
    await server.handleLine(JSON.stringify({ jsonrpc: "2.0", id, method: "tool/call", params: {
      name: "result", input: {}, lifetime: { total_ms: 800, idle_ms: 450 },
    } }))
    if (value === complete) expect(frames.at(-1)).toEqual({ jsonrpc: "2.0", id, result: complete })
    else {
      expect(validateToolResponse(value)).toBe(false)
      expect(frames.at(-1)).toMatchObject({ id, error: { code: -32603, message: "invalid tool response" } })
    }
    id++
  }
  await server.shutdown()
})
