import { describe, expect, test } from "bun:test"
import { MAX_COMMAND_REPLY_BYTES, PROTOCOL_VERSION, type ClientCommand } from "../src/protocol"
import { EngineHttpSseClient, EngineProtocolError } from "../src/transport"

const attach = {
  type: "attach_session",
  meta: { protocol_version: PROTOCOL_VERSION, client_id: "unbound", request_id: "attach" },
  session_id: "session", last_seen_sequence: null, role: "driver",
} satisfies ClientCommand

const invalidReplies = [
  { name: "missing body", response: () => new Response(null, { status: 204 }) },
  { name: "empty JSON body", response: () => new Response("", { headers: { "content-type": "application/json", "content-length": "0" } }) },
  { name: "non-JSON body", response: () => new Response("accepted", { headers: { "content-type": "text/plain" } }) },
  { name: "unwrapped outcome", response: () => Response.json({ type: "accepted" }) },
  { name: "malformed JSON", response: () => new Response("{", { headers: { "content-type": "application/json" } }) },
  { name: "malformed UTF-8", response: () => new Response(new Uint8Array([34, 0xff, 34]), { headers: { "content-type": "application/json" } }) },
  { name: "mismatched command class", response: () => Response.json({ type: "read", outcome: { type: "accepted" }, events: [] }) },
  { name: "declared oversized body", response: () => new Response("{}", { headers: { "content-type": "application/json", "content-length": String(MAX_COMMAND_REPLY_BYTES + 1) } }) },
]

describe("typed command response contract", () => {
  for (const { name, response } of invalidReplies) {
    test(`${name} rejects and settles without replay retry`, async () => {
      let reconnects = 0
      let commands = 0
      let eventStreams = 0
      const abort = new AbortController()
      const client = new EngineHttpSseClient({
        socketPath: "/unused.sock", bootstrapToken: "fixture",
        fetch: Object.assign(async (input: string | URL | Request) => {
          const path = new URL(String(input)).pathname
          if (path === "/v1/connect") return Response.json({ client_id: "reader", token: "token" })
          if (path === "/v1/command") { commands += 1; return response() }
          eventStreams += 1
          return new Response(null, { headers: { "content-type": "text/event-stream" } })
        }, { preconnect() { } }),
        scheduler: { async sleep() { reconnects += 1; throw new Error("unexpected replay retry") } },
      })
      try {
        await expect(client.postCommand(attach)).rejects.toBeInstanceOf(EngineProtocolError)
        await expect(client.subscribe({ attach, signal: abort.signal, onEvent() { throw new Error("unexpected event") } }))
          .rejects.toBeInstanceOf(EngineProtocolError)
        expect(commands).toBe(2)
        expect(eventStreams).toBe(0)
        expect(reconnects).toBe(0)
      } finally {
        abort.abort()
      }
    })
  }
})
