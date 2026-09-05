import { describe, expect, test } from "bun:test"
import { ClientDiagnostics } from "../src/client-diagnostics"
import { MAX_CLIENT_READS, PROTOCOL_VERSION, type ClientCommand, type CommandReply } from "../src/protocol"
import { ClientReadAdmission, MAX_CLIENT_READ_REQUEST_BYTES, MAX_RETAINED_CLIENT_READS } from "../src/transport/read-admission"

const accepted: CommandReply = { type: "read", outcome: { type: "accepted" }, events: [] }
const meta = (id: string) => ({ protocol_version: PROTOCOL_VERSION, client_id: "client", request_id: id })
const read = (id: string): ClientCommand => ({ type: "list_sessions", meta: meta(id) })
function pending() {
  const calls: ClientCommand[] = []
  const replies = new Map<string, PromiseWithResolvers<CommandReply>>()
  const execute = (command: ClientCommand): Promise<CommandReply> => {
    calls.push(command)
    const reply = Promise.withResolvers<CommandReply>()
    replies.set(command.meta.request_id, reply)
    return reply.promise
  }
  return { calls, replies, execute }
}
const tick = async () => { await Promise.resolve(); await Promise.resolve() }

describe("shared client read admission", () => {
  test("distinct features share the generated ceiling and FIFO while mutations bypass it", async () => {
    const queue = new ClientReadAdmission()
    const host = pending()
    const commands: ClientCommand[] = [read("sessions"), { type: "list_models", refresh: false, meta: meta("context"), session_id: "s" },
      { type: "list_runtime_services", meta: meta("services"), session_id: "s" },
      { type: "list_settings", meta: meta("cost"), session_id: "s" }]
    const reads = commands.map(command => queue.run(command, undefined, host.execute))
    expect(host.calls.map(command => command.meta.request_id)).toEqual(["sessions", "context"])
    expect(queue.usage.active).toBe(MAX_CLIENT_READS)
    const interrupt = queue.run({ type: "interrupt", meta: meta("interrupt"), session_id: "s" }, undefined, host.execute)
    expect(host.calls.at(-1)?.type).toBe("interrupt")
    expect(queue.usage.active).toBe(MAX_CLIENT_READS)
    host.replies.get("interrupt")!.resolve({ type: "command", outcome: { type: "accepted" } })
    await interrupt
    host.replies.get("context")!.resolve(accepted)
    await tick()
    expect(host.calls.at(-1)?.meta.request_id).toBe("services")
    host.replies.get("sessions")!.resolve(accepted)
    await tick()
    expect(host.calls.at(-1)?.meta.request_id).toBe("cost")
    host.replies.get("services")!.resolve(accepted)
    host.replies.get("cost")!.resolve(accepted)
    await Promise.all(reads)
    expect(queue.usage).toEqual({ active: 0, queued: 0, bytes: 0 })
  })

  test("queued cancellation reclaims admission and a running abort waits for executor settlement", async () => {
    const queue = new ClientReadAdmission()
    const host = pending()
    const scope = new AbortController()
    const running = [0, 1].map(id => queue.run(read(String(id)), scope.signal, host.execute))
    const before = queue.usage.bytes
    const queued = queue.run(read("queued"), scope.signal, host.execute)
    const rejected = queued.catch((error: unknown) => error)
    expect(queue.usage.bytes).toBeGreaterThan(before)
    scope.abort(new DOMException("scope changed", "AbortError"))
    expect(await rejected).toMatchObject({ name: "AbortError", message: "scope changed" })
    expect(queue.usage).toEqual({ active: MAX_CLIENT_READS, queued: 0, bytes: before })
    const next = queue.run(read("next"), undefined, host.execute)
    expect(host.calls).toHaveLength(MAX_CLIENT_READS)
    host.replies.get("0")!.resolve(accepted)
    await tick()
    expect(host.calls.at(-1)?.meta.request_id).toBe("next")
    host.replies.get("1")!.resolve(accepted)
    host.replies.get("next")!.resolve(accepted)
    await Promise.all([...running, next])
    expect(queue.usage.bytes).toBe(0)
  })

  test("request count and bytes remain bounded and recover after cancellation", async () => {
    const queue = new ClientReadAdmission()
    const host = pending()
    const scope = new AbortController()
    const tasks = Array.from({ length: MAX_RETAINED_CLIENT_READS }, (_, id) =>
      queue.run(read(String(id)), scope.signal, host.execute))
    const settled = Promise.allSettled(tasks)
    expect(queue.usage.active + queue.usage.queued).toBe(MAX_RETAINED_CLIENT_READS)
    expect(queue.usage.bytes).toBeLessThanOrEqual(MAX_CLIENT_READ_REQUEST_BYTES)
    await expect(queue.run(read("extra"), undefined, host.execute)).rejects.toThrow("count exhausted")
    scope.abort()
    for (const reply of host.replies.values()) reply.resolve(accepted)
    await settled
    expect(queue.usage).toEqual({ active: 0, queued: 0, bytes: 0 })
    const large: ClientCommand = { type: "search_sessions", meta: meta("large"), query: "\u0000".repeat(20_000), limit: 1 }
    const first = queue.run(large, undefined, host.execute)
    const charged = queue.usage.bytes
    expect(charged).toBeLessThanOrEqual(MAX_CLIENT_READ_REQUEST_BYTES)
    await expect(queue.run({ ...large, meta: meta("overflow") }, undefined, host.execute)).rejects.toThrow("byte allowance")
    expect(queue.usage.bytes).toBe(charged)
    host.replies.get("large")!.resolve(accepted)
    await first
    await expect(queue.run(read("small"), undefined, async () => accepted)).resolves.toEqual(accepted)
    expect(queue.usage.bytes).toBe(0)
  })

  test("queued request identity and nested payload are captured before caller mutation", async () => {
    const queue = new ClientReadAdmission()
    const host = pending()
    const blockers = ["a", "b"].map(id => queue.run(read(id), undefined, host.execute))
    const command = { type: "search_sessions", meta: meta("original"), query: "original query", limit: 1 } satisfies ClientCommand
    const queued = queue.run(command, undefined, host.execute)
    const charged = queue.usage.bytes
    command.meta.request_id = "mutated"
    command.query = "x".repeat(MAX_CLIENT_READ_REQUEST_BYTES)
    expect(queue.usage.bytes).toBe(charged)
    host.replies.get("a")!.resolve(accepted)
    await tick()
    expect(host.calls.at(-1)).toEqual({ type: "search_sessions", meta: meta("original"), query: "original query", limit: 1 })
    host.replies.get("b")!.resolve(accepted)
    host.replies.get("original")!.resolve(accepted)
    await Promise.all([...blockers, queued])
  })

  test("pre-aborted reads and synchronous executor failures cannot strand a slot", async () => {
    const queue = new ClientReadAdmission()
    let called = false
    await expect(queue.run(read("aborted"), AbortSignal.abort(), async () => { called = true; return accepted })).rejects.toThrow()
    expect(called).toBe(false)
    await expect(queue.run(read("throws"), undefined, () => { throw new Error("executor failed") })).rejects.toThrow("executor failed")
    expect(queue.usage).toEqual({ active: 0, queued: 0, bytes: 0 })
    await expect(queue.run(read("works"), undefined, async () => accepted)).resolves.toEqual(accepted)
  })
  test("opt-in queue timing attributes admission delay without retaining payloads", async () => {
    let now = 0
    const diagnostics = new ClientDiagnostics(() => now)
    const queue = new ClientReadAdmission(diagnostics)
    const host = pending()
    const requests = ["a", "b", "c"].map(id => queue.run(read(id), undefined, host.execute))
    now = 40
    host.replies.get("a")!.resolve(accepted)
    await tick()
    const timing = diagnostics.snapshot().stages.find(stage => stage.stage === "read_queue_age")!
    expect(timing.count).toBe(3)
    expect(timing.totalMs).toBe(40)
    expect(timing.maxMs).toBe(40)
    host.replies.get("b")!.resolve(accepted)
    host.replies.get("c")!.resolve(accepted)
    await Promise.all(requests)
  })

})
