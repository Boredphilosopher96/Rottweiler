import { describe, expect, test } from "bun:test"
import { PROTOCOL_VERSION, type ClientCommand, type EngineEvent, type CommandReply } from "../src/protocol"
import { EngineHttpSseClient } from "../src/transport"
import { boundedJson } from "../src/transport/json"

const query = {
  type: "list_sessions",
  meta: { protocol_version: PROTOCOL_VERSION, client_id: "unbound", request_id: "query" },
} satisfies ClientCommand
const listed = {
  type: "sessions_listed",
  meta: { protocol_version: PROTOCOL_VERSION, client_id: "reader", request_id: "query", emitted_at: "2026-09-04T00:00:00Z" },
  sessions: [],
} satisfies EngineEvent

function clientFor(reply: unknown): EngineHttpSseClient {
  return new EngineHttpSseClient({
    socketPath: "/unused.sock", bootstrapToken: "fixture",
    fetch: Object.assign(async (input: string | URL | Request) => new URL(String(input)).pathname === "/v1/connect"
      ? Response.json({ client_id: "reader", token: "token" })
      : Response.json(reply), { preconnect() {} }),
  })
}

describe("direct query reply boundary", () => {
  test("reply discriminator follows the source-owned command execution class", async () => {
    await expect(clientFor({ type: "command", outcome: { type: "accepted" } }).postCommand(query))
      .rejects.toThrow("reply class")
    const control = { type: "interrupt", meta: query.meta, session_id: "session" } satisfies ClientCommand
    await expect(clientFor({ type: "read", outcome: { type: "accepted" }, events: [] }).postCommand(control))
      .rejects.toThrow("reply class")
  })
  test("accepts correlated typed query data without an SSE acknowledgement", async () => {
    const reply = { type: "read", outcome: { type: "accepted" }, events: [listed] } satisfies CommandReply
    expect(await clientFor(reply).postCommand(query)).toEqual(reply)
  })
  test("rejects query events from a different bound session", async () => {
    const command = { ...query, type: "list_commands", session_id: "expected" } satisfies ClientCommand
    const event = { type: "command_descriptors_listed", meta: listed.meta, session_id: "foreign", commands: [], truncated: false } satisfies EngineEvent
    await expect(clientFor({ type: "read", outcome: { type: "accepted" }, events: [event] }).postCommand(command)).rejects.toThrow()
  })
  test("rejects malformed known data, durable data, and foreign request identity", async () => {
    const invalid = [
      { ...listed, sessions: "broken" },
      { type: "text_delta", meta: { protocol_version: PROTOCOL_VERSION, session_id: "session", sequence_id: "0", emitted_at: "now" }, turn_id: "1", text: "injected" },
      { ...listed, meta: { ...listed.meta, request_id: "another" } },
      { ...listed, meta: { ...listed.meta, client_id: "another" } },
      { ...listed, meta: { ...listed.meta, protocol_version: PROTOCOL_VERSION + 1 } },
    ]
    for (const event of invalid) {
      await expect(clientFor({ type: "read", outcome: { type: "accepted" }, events: [event] }).postCommand(query)).rejects.toThrow()
    }
  })
})

describe("byte-owned fragmented JSON decoding", () => {
  test("preserves multibyte text across one-byte fragmentation", async () => {
    const value = { text: "λ🐕".repeat(10_000) }
    const bytes = new TextEncoder().encode(JSON.stringify(value))
    let offset = 0
    const response = new Response(new ReadableStream<Uint8Array>({
      pull(controller) {
        if (offset === bytes.length) controller.close()
        else controller.enqueue(bytes.subarray(offset, ++offset))
      },
    }))
    expect(await boundedJson(response, bytes.length)).toEqual(value)
  })
  test("cancels a streaming response as soon as actual bytes exceed the limit", async () => {
    let cancelled = false
    const response = new Response(new ReadableStream<Uint8Array>({
      pull(controller) { controller.enqueue(new Uint8Array(33)) },
      cancel() { cancelled = true },
    }))
    await expect(boundedJson(response, 64)).rejects.toThrow("byte limit")
    expect(cancelled).toBe(true)
  })
  test("preserves cancellation when fetch closes the body after response headers", async () => {
    const lifetime = new AbortController()
    const reason = new DOMException("history selection changed", "AbortError")
    const response = new Response(new ReadableStream<Uint8Array>({
      pull(controller) { lifetime.abort(reason); controller.close() },
    }), { headers: { "content-length": "226" } })
    await expect(boundedJson(response, 1024, undefined, lifetime.signal)).rejects.toBe(reason)
  })
  test("rejects truncated non-cancelled replies even when their prefix is valid JSON", async () => {
    const response = new Response("{}", { headers: { "content-length": "226" } })
    await expect(boundedJson(response, 1024)).rejects.toThrow("Content-Length")
  })
  test("rejects declared overflow and malformed UTF-8", async () => {
    let cancelled = false
    const declared = new Response(new ReadableStream<Uint8Array>({
      cancel() { cancelled = true },
    }), { headers: { "content-length": "9999" } })
    await expect(boundedJson(declared, 64)).rejects.toThrow("byte limit")
    expect(cancelled).toBe(true)
    await expect(boundedJson(new Response(new Uint8Array([34, 0xff, 34])), 64)).rejects.toThrow()
  })
})
