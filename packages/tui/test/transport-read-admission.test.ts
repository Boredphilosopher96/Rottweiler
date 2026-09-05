import { describe, expect, test } from "bun:test"
import { EngineHttpSseClient } from "../src/transport"
import { CLIENT_COMMAND_EXECUTION, MAX_CLIENT_READS, PROTOCOL_VERSION, type ClientCommand, type CommandReply } from "../src/protocol"

function harness() {
  const commands: ClientCommand[] = []
  const bodies = new Map<string, ReadableStreamDefaultController<Uint8Array>>()
  const closed: string[] = []
  const client = new EngineHttpSseClient({ socketPath: "/tmp/read-admission.sock", bootstrapToken: "bootstrap",
    fetch: (async (input: string | URL | Request, init?: RequestInit) => {
      if (String(input).endsWith("/v1/connect")) return Response.json({ client_id: "bound", token: "token" })
      const command = JSON.parse(String(init?.body)) as ClientCommand
      commands.push(command)
      const id = command.meta.request_id
      if (CLIENT_COMMAND_EXECUTION[command.type] !== "read") return Response.json({ type: "command", outcome: { type: "accepted" } } satisfies CommandReply)
      return new Response(new ReadableStream<Uint8Array>({
        start(controller) {
          bodies.set(id, controller)
          const abort = () => { closed.push(id); controller.error(init?.signal?.reason) }
          init?.signal?.addEventListener("abort", abort, { once: true })
        },
        cancel() { closed.push(id) },
      }), { headers: { "Content-Type": "application/json" } })
    }) as typeof fetch,
  })
  function finish(id: string) {
    const body = bodies.get(id)!
    body.enqueue(new TextEncoder().encode(JSON.stringify({ type: "read", outcome: { type: "accepted" }, events: [] } satisfies CommandReply)))
    body.close()
  }
  return { client, commands, closed, finish }
}
const read = (id: string): ClientCommand => ({ type: "list_sessions", meta: { protocol_version: PROTOCOL_VERSION, client_id: "unbound", request_id: id } })
async function waitFor(predicate: () => boolean) {
  for (let remaining = 1000; !predicate(); remaining--) {
    if (remaining === 0) throw new Error("request did not reach the transport")
    await Bun.sleep(1)
  }
}

describe("HTTP read admission lifetime", () => {
  test("response headers do not release the shared slot; mutations proceed while bodies wait", async () => {
    const host = harness()
    const reads = ["a", "b", "c"].map(id => host.client.postCommand(read(id)))
    await waitFor(() => host.commands.length === MAX_CLIENT_READS)
    await host.client.postCommand({ type: "interrupt", meta: read("interrupt").meta, session_id: "s" })
    expect(host.commands.map(command => command.meta.request_id)).toEqual(["a", "b", "interrupt"])
    host.finish("a")
    await reads[0]
    await waitFor(() => host.commands.length === 4)
    expect(host.commands.at(-1)?.meta.request_id).toBe("c")
    host.finish("b")
    host.finish("c")
    await Promise.all(reads)
  })

  test("scope abort cancels running bodies and queued reads without posting abandoned requests", async () => {
    const host = harness()
    const scope = new AbortController()
    const reads = ["a", "b", "queued"].map(id => host.client.postCommand(read(id), scope.signal))
    const settled = Promise.allSettled(reads)
    await waitFor(() => host.commands.length === MAX_CLIENT_READS)
    scope.abort(new DOMException("session changed", "AbortError"))
    const results = await settled
    expect(results.every(result => result.status === "rejected")).toBe(true)
    expect(host.commands.map(command => command.meta.request_id)).toEqual(["a", "b"])
    expect(host.closed.sort()).toEqual(["a", "b"])
    const fresh = host.client.postCommand(read("fresh"))
    await waitFor(() => host.commands.length === 3)
    host.finish("fresh")
    await expect(fresh).resolves.toMatchObject({ type: "read", outcome: { type: "accepted" } })
  })
})
