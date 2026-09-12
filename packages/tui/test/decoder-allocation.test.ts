import { expect, test } from "bun:test"
import { CLIENT_ALLOCATION_LIMITS, ClientAllocationOwner } from "../src/client-allocation"
import { EngineHttpSseClient, parseSseStream, SseLimitError } from "../src/transport"
import { PROTOCOL_VERSION } from "../src/protocol"

const tick = async (ready: () => boolean) => {
  for (let i = 0; i < 100 && !ready(); i++) await Bun.sleep(1)
  expect(ready()).toBeTrue()
}

test("shared SSE refusal precedes event decoding and holds credit until cancellation settles", async () => {
  const owner = new ClientAllocationOwner(CLIENT_ALLOCATION_LIMITS, 64 * 1024)
  const history = owner.reserve("history", 16 * 1024), claim = owner.reserve("decoding", 0)
  let cancelStarted = false, releaseCancel!: () => void, delivered = 0, reads = 0
  const bytes = new TextEncoder().encode(`data: ${JSON.stringify({ type: "text_delta", meta: {
    protocol_version: PROTOCOL_VERSION, session_id: "s", sequence_id: "1", emitted_at: "2026-01-01T00:00:00Z",
  }, turn_id: "1", text: "x".repeat(8192) })}\n\n`)
  const client = new EngineHttpSseClient({ socketPath: "/private/test.sock", bootstrapToken: "bootstrap", fetch: (async input => {
    const path = new URL(String(input)).pathname
    if (path === "/v1/connect") return Response.json({ client_id: "c", token: "token" })
    if (path === "/v1/command") return Response.json({ type: "command", outcome: { type: "accepted" } })
    reads++
    return new Response(new ReadableStream<Uint8Array>({ start(controller) { controller.enqueue(bytes) },
      cancel() { cancelStarted = true; return new Promise<void>(resolve => { releaseCancel = resolve }) },
    }), { headers: { "content-type": "text/event-stream" } })
  }) as typeof fetch, scheduler: { async sleep() { throw new Error("deterministic refusal must not retry") } } })
  const completion = client.subscribe({ attach: { type: "attach_session", meta: { protocol_version: PROTOCOL_VERSION, client_id: "c", request_id: "r" },
    session_id: "s", role: "driver", last_seen_sequence: null }, signal: new AbortController().signal,
    allocation: { admit: bytes => claim.resize(bytes) }, onEvent() { delivered++ },
  })
  const result = completion.catch(error => error)
  await tick(() => cancelStarted)
  expect(claim.bytes).toBeGreaterThan(0)
  expect(owner.usage.bytes).toBe(history.bytes + claim.bytes)
  expect(delivered).toBe(0)
  releaseCancel()
  expect(await result).toBeInstanceOf(SseLimitError)
  expect(claim.bytes).toBe(0)
  expect(reads).toBe(1)
  claim.release(); history.release()
  expect(owner.usage.bytes).toBe(0)
})

test("already-aborted stream cancellation releases both decoder credit and its reader on rejection", async () => {
  const owner = new ClientAllocationOwner(), claim = owner.reserve("decoding", 1024)
  const signal = new AbortController(); signal.abort()
  const source = new ReadableStream<Uint8Array>({ cancel() { return Promise.reject(new Error("cancel failure")) } })
  const iterator = parseSseStream(source, { allocation: { admit: bytes => claim.resize(bytes) } }, signal.signal)
  await expect(iterator.next()).rejects.toThrow("cancel failure")
  expect(source.locked).toBeFalse()
  expect(claim.bytes).toBe(0)
  claim.release()
})

for (const operation of ["bootstrap", "credential", "activation"] as const) {
  for (const cancellation of ["resolve", "reject"] as const) {
    test(`${operation} response disposal awaits ${cancellation} before releasing the operation`, async () => {
      const owner = new ClientAllocationOwner()
      const cancelled = Promise.withResolvers<void>(), settlement = Promise.withResolvers<void>()
      const client = new EngineHttpSseClient({ socketPath: "/private/test.sock", bootstrapToken: "bootstrap", fetch: (async input => {
        if (String(input).endsWith("/v1/connect") && operation !== "bootstrap") {
          return Response.json({ client_id: "c", token: "token" })
        }
        return new Response(new ReadableStream<Uint8Array>({
          cancel() { cancelled.resolve(); return settlement.promise },
        }), { status: operation === "activation" ? 200 : 401 })
      }) as typeof fetch })
      let completed = false
      const pending = (async () => {
        using allocation = owner.reserve("decoding", 0)
        if (operation === "activation") await client.activateProvider("s", "provider", undefined, allocation)
        else await client.submitProviderApiKey("s", "provider", "key", undefined, allocation)
      })().catch(error => error).finally(() => { completed = true })
      await cancelled.promise
      expect(completed).toBeFalse()
      if (operation !== "bootstrap") expect(owner.usage.domains.decoding).toBeGreaterThan(0)
      if (cancellation === "reject") settlement.reject(new Error("response disposal failed"))
      else settlement.resolve()
      const result = await pending
      if (operation === "activation" && cancellation === "resolve") expect(result).toBeUndefined()
      else expect(result).toBeInstanceOf(Error)
      expect(owner.usage.bytes).toBe(0)
    })
  }
}
