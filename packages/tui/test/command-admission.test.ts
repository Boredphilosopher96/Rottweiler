import { expect, test } from "bun:test"
import { MAX_CLIENT_CONTROLS, MAX_CLIENT_URGENT_CONTROLS, MAX_IMAGE_ATTACHMENT_BYTES } from "../../../protocol/types"
import { ClientAllocationOwner } from "../src/client-allocation"
import { ClientCache } from "../src/history/cache"
import { ClientCommandAdmission } from "../src/transport/command-admission"
import { EngineHttpSseClient } from "../src/transport"
import { PROTOCOL_VERSION, type ClientCommand, type CommandReply } from "../src/protocol"

const meta = (id: string) => ({ protocol_version: PROTOCOL_VERSION, client_id: "client", request_id: id })
const message = (id: string): ClientCommand => ({ type: "send_message", session_id: "s", meta: meta(id), content: "draft", attachments: [] })
const interrupt = (id: string): ClientCommand => ({ type: "interrupt", session_id: "s", meta: meta(id) })
const accepted: CommandReply = { type: "command", outcome: { type: "accepted" } }

test("normal and urgent controls own independent counts through real executor settlement", async () => {
  const owner = new ClientAllocationOwner(), admission = new ClientCommandAdmission(undefined, owner)
  const completed = Promise.withResolvers<CommandReply>(), abort = new AbortController()
  const snapshots: ClientCommand[] = []
  const execute = async (command: ClientCommand) => { snapshots.push(command); return completed.promise }
  const original = message("original")
  const normal = [admission.run(original, abort.signal, execute), ...Array.from({ length: MAX_CLIENT_CONTROLS - 1 }, (_, index) => admission.run(message(String(index)), abort.signal, execute))]
  const held = owner.usage.bytes
  if (original.type === "send_message") original.content = "changed outside the admitted snapshot"
  expect(snapshots[0]).toMatchObject({ content: "draft" })
  await expect(admission.run(message("overflow"), undefined, execute)).rejects.toThrow("count exhausted")
  abort.abort()
  expect(owner.usage.bytes).toBe(held)
  expect(admission.controlUsage.normal).toBe(MAX_CLIENT_CONTROLS)
  const urgent = Array.from({ length: MAX_CLIENT_URGENT_CONTROLS }, (_, index) => admission.run(interrupt(String(index)), undefined, execute))
  await expect(admission.run(interrupt("overflow"), undefined, execute)).rejects.toThrow("count exhausted")
  expect(admission.controlUsage.urgent).toBe(MAX_CLIENT_URGENT_CONTROLS)
  completed.resolve(accepted)
  await Promise.all([...normal, ...urgent])
  expect(admission.controlUsage).toEqual({ normal: 0, urgent: 0 })
  expect(owner.usage.bytes).toBe(0)
})

test("urgent HTTP commands retain request and response capacity under full normal pressure", async () => {
  const owner = new ClientAllocationOwner(), cache = new ClientCache<{ title: string }>(undefined, owner)
  cache.insert("mounted", { title: "keep visible" })
  const view = cache.lease("mounted")!
  const live = owner.reserve("live", owner.limits.live)
  const pressure = owner.reserve("outbound", owner.normalCapacity - owner.usage.bytes)
  const read = Promise.withResolvers<void>(), bodies: ReadableStreamDefaultController<Uint8Array>[] = []
  let fetches = 0
  const client = new EngineHttpSseClient({ allocations: owner, socketPath: "/private/test.sock", bootstrapToken: "bootstrap", fetch: (async input => {
    fetches++
    if (String(input).endsWith("/v1/connect")) return Response.json({ client_id: "client", token: "token" })
    return new Response(new ReadableStream<Uint8Array>({ start(body) { bodies.push(body); if (bodies.length === MAX_CLIENT_URGENT_CONTROLS) read.resolve() } }), {
      headers: { "content-type": "application/json" },
    })
  }) as typeof fetch })
  try {
    const held = owner.usage.bytes
    await expect(client.postCommand(message("refused"))).rejects.toThrow("admission")
    expect(fetches).toBe(0)
    expect(owner.usage.bytes).toBe(held)
    expect(view.value.title).toBe("keep visible")
    const urgent = Array.from({ length: MAX_CLIENT_URGENT_CONTROLS }, (_, index) => (async () => {
      using reply = owner.reserve("urgent", 0)
      return await client.postCommand(interrupt(String(index)), undefined, reply)
    })())
    await read.promise
    expect(owner.usage.bytes).toBeGreaterThan(held)
    expect(owner.usage.bytes).toBeLessThanOrEqual(owner.maximumBytes)
    expect(owner.usage.domains.urgent).toBeGreaterThan(0)
    for (const body of bodies) { body.enqueue(new TextEncoder().encode(JSON.stringify(accepted))); body.close() }
    expect(await Promise.all(urgent)).toEqual([accepted, accepted])
    expect(owner.usage.bytes).toBe(held)
    expect(view.value.title).toBe("keep visible")
  } finally { pressure.release(); live.release(); view.release(); cache.clear() }
  expect(owner.usage.bytes).toBe(0)
})

test("two maximum image attachments fit measured capture and encoded request ownership", async () => {
  const owner = new ClientAllocationOwner(), admission = new ClientCommandAdmission(undefined, owner)
  const encoded = Buffer.alloc(MAX_IMAGE_ATTACHMENT_BYTES).toString("base64")
  const command: ClientCommand = { type: "send_message", meta: meta("images"), session_id: "s", content: "inspect", attachments: [0, 1].map(index => ({ name: `${index}.png`, media_type: "image/png", data: { type: "inline_base64", data: encoded } })) }
  await expect(admission.run(command, undefined, async (snapshot, _signal, prepare) => {
    prepare({ ...snapshot, meta: { ...snapshot.meta, client_id: "authenticated-client" } })
    expect(owner.usage.domains.outbound).toBeGreaterThan(encoded.length * 4)
    expect(owner.usage.bytes).toBeLessThan(owner.limits.outbound)
    return accepted
  })).resolves.toEqual(accepted)
  expect(owner.usage.bytes).toBe(0)
})


test("a conditional read watch leaves both ordinary read slots and urgent controls available", async () => {
  const owner = new ClientAllocationOwner(), admission = new ClientCommandAdmission(undefined, owner)
  const waited = Promise.withResolvers<CommandReply>(), read = Promise.withResolvers<CommandReply>(), abort = new AbortController()
  const watch: ClientCommand = { type: "read_family_controls", meta: meta("watch"), session_id: "s", after_revision: "9" }
  const waiting = admission.run(watch, abort.signal, async () => waited.promise)
  await expect(admission.run({ ...watch, meta: meta("duplicate") }, undefined, async () => accepted)).rejects.toThrow("watch count")
  const reading = Array.from({ length: 2 }, (_, index) => admission.run({ type: "get_cost", meta: meta(String(index)), session_id: "s" }, undefined, async () => read.promise))
  expect(admission.usage.active).toBe(2)
  expect(admission.watchUsage).toBe(1)
  await expect(admission.run(interrupt("interrupt"), undefined, async () => accepted)).resolves.toEqual(accepted)
  const held = owner.usage.bytes
  abort.abort()
  expect(owner.usage.bytes).toBe(held)
  expect(admission.watchUsage).toBe(1)
  read.resolve(accepted)
  await Promise.all(reading)
  expect(admission.usage.active).toBe(0)
  expect(owner.usage.bytes).toBeGreaterThan(0)
  waited.resolve(accepted)
  await waiting
  expect(admission.watchUsage).toBe(0)
  expect(owner.usage.bytes).toBe(0)
})
