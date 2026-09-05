import { expect, test } from "bun:test"
import { contractFixture } from "../../../protocol/fixtures/contract"
import { PROTOCOL_VERSION, type ClientCommand, type CommandReply } from "../src/protocol"
import { EngineHttpSseClient, EngineProtocolError } from "../src/transport"

const attach = {
  type: "attach_session",
  meta: { protocol_version: PROTOCOL_VERSION, client_id: "unbound", request_id: "attach" },
  session_id: "session", last_seen_sequence: "0", role: "driver",
} satisfies ClientCommand
const text = contractFixture.engine_events.find(event => event.type === "text_delta")
if (text === undefined) throw new Error("missing text fixture")

for (const [name, event] of [
  ["unsupported discriminator", { type: "undeclared_event", meta: { sequence_id: "1" } }],
  ["undeclared event field", { ...text, undeclared: true }],
  ["undeclared metadata field", { ...text, meta: { ...text.meta, undeclared: true } }],
]) {
  test(`${name} closes its stream without delivery, cursor advance or retry`, async () => {
    let streams = 0
    let delivered = 0
    let retries = 0
    let cancelled = false
    const cursors: unknown[] = []
    const abort = new AbortController()
    const client = new EngineHttpSseClient({
      socketPath: "/unused.sock", bootstrapToken: "fixture",
      fetch: Object.assign(async (input: string | URL | Request, init?: RequestInit) => {
        const path = new URL(String(input)).pathname
        if (path === "/v1/connect") return Response.json({ client_id: "reader", token: "token" })
        if (path === "/v1/command") {
          const command = JSON.parse(String(init?.body))
          cursors.push(command.last_seen_sequence)
          return Response.json({ type: "command", outcome: { type: "accepted" } } satisfies CommandReply)
        }
        streams += 1
        return new Response(new ReadableStream({
          start(controller) { controller.enqueue(new TextEncoder().encode(`data: ${JSON.stringify(event)}\n\n`)) },
          cancel() { cancelled = true },
        }), { headers: { "content-type": "text/event-stream" } })
      }, { preconnect() {} }),
      scheduler: { async sleep() { retries += 1; throw new Error("unexpected replay retry") } },
    })
    try {
      await expect(client.subscribe({ attach, signal: abort.signal, onEvent() { delivered += 1 } }))
        .rejects.toBeInstanceOf(EngineProtocolError)
      expect(streams).toBe(1)
      expect(delivered).toBe(0)
      expect(retries).toBe(0)
      expect(cursors).toEqual(["0"])
      expect(cancelled).toBe(true)
    } finally { abort.abort() }
  })
}
