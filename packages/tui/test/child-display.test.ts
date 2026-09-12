import { expect, test } from "bun:test"
import { ChildDisplayController } from "../src/child-display"
import { ClientCache } from "../src/history/cache"
import type { HistoryCacheValue } from "../src/history/controller"
import { TRANSCRIPT_PROJECTION_VERSION, type SessionStateSnapshot, type TranscriptTailPart, type TranscriptTailPage } from "../src/protocol"
import { descendantSessionRead, directSessionRead } from "../src/session-reader"

const target = { session_id: "child", ancestry: [{ subagent_id: "agent", session_id: "child" }] }
const source = descendantSessionRead(directSessionRead("root"), { subagent_id: "agent", session_id: "child", source_sequence: "2" })
const flush = () => new Promise<void>(resolve => setImmediate(resolve))
function metadata(through = "10"): SessionStateSnapshot {
  return { through, driver_client_id: "driver", title: "Child", model_alias: "main", provider: null, thinking: "off", mode_id: "execute",
    active_turn: { turn_id: "actual-turn", started: "5" }, completed_turns: "1", shell: null, compaction: null, plugin_statuses: [], queued_messages: [], budget: null }
}
function page(part: TranscriptTailPart, through = "10", started = "5"): TranscriptTailPage {
  return { identity: { generation: "0", turn_started: started, response_epoch: "5", tools_epoch: "5" },
    view: { session_id: "child", projection_version: TRANSCRIPT_PROJECTION_VERSION, generation: "0", through, digest: Array(32).fill(0) as TranscriptTailPage["view"]["digest"] },
    content: part.type === "text" || part.type === "thinking" ? { type: part.type, preview: { text: part.type === "text" ? "current child text" : "", truncated: false } }
      : { type: part.type, offset: part.offset, items: [], next_offset: null },
  }
}

test("selected child reads retain incoming pages through presentation and use the actor turn identity", async () => {
  const cache = new ClientCache<HistoryCacheValue>()
  const done = Promise.withResolvers<void>()
  let reads = 0
  const controller = new ChildDisplayController({ cache,
    async readState(root, selected, _signal, allocation) { expect(root).toBe("root"); expect(selected).toEqual(target); allocation.admit(8192); return metadata() },
    async readTail(selected, request, _signal, allocation) { expect(selected).toEqual(source); reads++; allocation.admit(4096); return { type: "ready", page: page(request.part) } },
    apply(snapshot, pages) {
      expect(snapshot.active_turn?.turn_id).toBe("actual-turn")
      expect(pages).toHaveLength(4)
      expect(cache.usage.pinnedEntries).toBe(4)
      expect(cache.allocations.usage.domains.metadata).toBeGreaterThanOrEqual(8192)
      controller.close(); done.resolve()
    }, failed(message) { if (message !== null) throw new Error(message) },
  })
  controller.open("root", target, source)
  await done.promise; await controller.settled()
  expect(reads).toBe(4)
  expect(cache.allocations.usage.bytes).toBe(0)
})

for (const invalid of ["turn", "prefix"] as const) {
  test(`selected display refuses a mismatched ${invalid} without replacing controls or state`, async () => {
    const cache = new ClientCache<HistoryCacheValue>(), failed = Promise.withResolvers<string>()
    let applied = 0
    const controller = new ChildDisplayController({ cache,
      async readState() { return metadata() },
      async readTail(_selected, request) { return { type: "ready", page: page(request.part, invalid === "prefix" ? "9" : "10", invalid === "turn" ? "6" : "5") } },
      apply() { applied++ }, failed(message) { if (message !== null) { controller.close(); failed.resolve(message) } },
    })
    controller.open("root", target, source)
    expect(await failed.promise).toContain(invalid === "turn" ? "active turn" : "predates")
    await controller.settled()
    expect(applied).toBe(0)
    expect(cache.allocations.usage.bytes).toBe(0)
  })
}

test("rapid child switches wait for canceled metadata reads to settle and preserve their binding credit", async () => {
  const cache = new ClientCache<HistoryCacheValue>(), pending = Promise.withResolvers<void>()
  const calls: string[] = []
  let applied = false
  const controller = new ChildDisplayController({ cache,
    async readState(_root, selected, _signal, allocation) { calls.push(selected.session_id); allocation.admit(8192); await pending.promise; return metadata() },
    async readTail() { throw new Error("canceled metadata cannot begin tail reads") },
    apply() { applied = true }, failed(message) { if (message !== null) throw new Error(message) },
  })
  controller.open("root", target, source)
  await flush()
  for (let index = 0; index < 100; index++) controller.open("root", { ...target, session_id: `child-${index}` }, source)
  expect(calls).toEqual(["child"])
  controller.close()
  expect(cache.allocations.usage.domains.metadata).toBe(8192)
  expect(cache.allocations.usage.domains.children).toBeGreaterThan(0)
  pending.resolve(); await controller.settled()
  expect(applied).toBe(false)
  expect(cache.allocations.usage.bytes).toBe(0)
})

test("compaction preview revisions refresh independently without rereading an unchanged durable tail", async () => {
  const cache = new ClientCache<HistoryCacheValue>(), done = Promise.withResolvers<void>()
  let states = 0, tails = 0, applies = 0
  const controller = new ChildDisplayController({ cache,
    async readState() {
      states++
      return { ...metadata(), compaction: { started: "8", summary_turn_id: "summary", revision: String(states), attempt: 1,
        text: { text: `summary ${states}`, truncated: false }, thinking: { text: "", truncated: false } } }
    },
    async readTail(_target, request) { tails++; return { type: "ready", page: page(request.part) } },
    apply(snapshot, pages) {
      applies++
      if (applies === 1) expect(pages).toHaveLength(4)
      else {
        expect(snapshot.compaction?.text.text).toBe("summary 2")
        expect(pages).toBeNull()
        controller.close(); done.resolve()
      }
    }, failed(message) { if (message !== null) throw new Error(message) },
  })
  controller.open("root", target, source)
  await done.promise; await controller.settled()
  expect(tails).toBe(4)
  expect(cache.allocations.usage.bytes).toBe(0)
})

test("a refused display and error render settle the collector without retrying into a failing renderer", async () => {
  const cache = new ClientCache<HistoryCacheValue>(), failure = Promise.withResolvers<void>()
  let applied = 0, reported = 0
  const controller = new ChildDisplayController({ cache,
    async readState() { return metadata() },
    async readTail(_target, request) { return { type: "ready", page: page(request.part) } },
    apply() { applied++; throw new Error("projection admission refused") },
    failed(message) { expect(message).toBe("projection admission refused"); reported++; failure.resolve(); throw new Error("error renderer refused") },
  })
  controller.open("root", target, source)
  await failure.promise; await controller.settled()
  expect(applied).toBe(1); expect(reported).toBe(1)
  expect(cache.allocations.usage.bytes).toBe(0)
})
