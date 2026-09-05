import { expect, test } from "bun:test"
import { ClientCache } from "../src/history/cache"
import { HistoryController, type HistoryCacheValue } from "../src/history/controller"
import type { HistoryReader } from "../src/history/reader"
import type { TranscriptPage, TranscriptReadResult } from "../src/protocol"
import { TRANSCRIPT_PROJECTION_VERSION } from "../src/protocol"

function page(session: string, first: number, text = "body", generation = "0", through = "1000"): TranscriptPage {
  return {
    view: { session_id: session, projection_version: TRANSCRIPT_PROJECTION_VERSION, generation, through, digest: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0] },
    first_ordinal: String(first), total_items: "1000", anchor: { type: "unspecified" }, invalidation: { type: "none" },
    items: [{
      id: String(first), ordinal: String(first), revision: through, agent_turn: null,
      content: {
        type: "command", name: "fixture", message: {
          text, format: "text", complete: true,
          source: { sequence: String(first), selector: { type: "command_message" } }
        }
      }
    }],
  }
}
function reader(read: HistoryReader["page"]): HistoryReader {
  return { page: read, content: async () => { throw new Error("unused content") } }
}
function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>(settle => { resolve = settle })
  return { promise, resolve }
}

test("late responses from superseded requests cannot resurrect another session", async () => {
  const first = deferred<TranscriptReadResult>()
  const second = deferred<TranscriptReadResult>()
  const controller = new HistoryController(reader(session => session === "first" ? first.promise : second.promise), () => { })
  const stale = controller.open("first")
  const current = controller.open("second")
  second.resolve({ type: "ready", page: page("second", 2) })
  await current
  first.resolve({ type: "ready", page: page("first", 1) })
  await stale
  expect(controller.snapshot.sessionId).toBe("second")
  expect(controller.snapshot.page?.items[0]?.id).toBe("2")
  expect(controller.cache.usage.entries).toBe(1)
  controller.dispose()
  expect(controller.cache.usage.bytes).toBe(0)
})

test("evicted pages are restored through bounded ordinal reads", async () => {
  let requests = 0
  const cache = new ClientCache<HistoryCacheValue>({ bytes: 12_000, entries: 3 })
  const controller = new HistoryController(reader(async (session, request) => {
    requests++
    const ordinal = request.position.type === "at_ordinal" ? Number(request.position.ordinal) : 999
    return { type: "ready", page: page(session, ordinal, "x".repeat(1000)) }
  }), () => { }, cache)
  await controller.open("session")
  for (let ordinal = 0; ordinal < 100; ordinal++) {
    await controller.seek(BigInt(ordinal))
    expect(cache.usage.bytes).toBeLessThanOrEqual(12_000)
    expect(cache.usage.entries).toBeLessThanOrEqual(3)
  }
  const beforeRestore = requests
  await controller.seek(0n)
  expect(requests).toBe(beforeRestore + 1)
  expect(controller.snapshot.page?.items[0]?.id).toBe("0")
  await controller.seek(0n)
  expect(requests).toBe(beforeRestore + 1)
  controller.dispose()
})

test("late same-generation revisions invalidate cached rows before replacement", async () => {
  let updated = false
  const controller = new HistoryController(reader(async session => {
    const result = page(session, 2, updated ? "final" : "running", "0", updated ? "1001" : "1000")
    if (updated) result.invalidation = { type: "items", items: ["2"] }
    return { type: "ready", page: result }
  }), () => { })
  await controller.open("session")
  updated = true
  await controller.refresh()
  expect(controller.snapshot.page?.items[0]?.content).toMatchObject({ message: { text: "final" } })
  expect(controller.cache.usage.entries).toBe(1)
  expect(controller.cache.usage.pinnedEntries).toBe(1)
  controller.dispose()
})

test("rewind retries around the stable anchor and uses its surviving replacement", async () => {
  let reads = 0
  const positions: string[] = []
  const controller = new HistoryController(reader(async (session, request) => {
    reads++
    positions.push(request.position.type)
    if (reads === 1) return { type: "ready", page: page(session, 9) }
    if (reads === 2) return { type: "ordering_changed", view: page(session, 3, "body", "1").view }
    const result = page(session, 3, "surviving", "1")
    result.anchor = { type: "replaced", requested: "9", replacement: "3" }
    return { type: "ready", page: result }
  }), () => { })
  await controller.open("session")
  await controller.seek(500n)
  expect(positions).toEqual(["latest", "at_ordinal", "around"])
  expect(controller.snapshot.page?.anchor).toEqual({ type: "replaced", requested: "9", replacement: "3" })
  controller.dispose()
})

test("malformed semantic ranges never enter the retained cache", async () => {
  const bad = page("session", 1)
  const first = bad.items[0]
  if (first === undefined) throw new Error("fixture item missing")
  first.ordinal = "-1"
  const controller = new HistoryController(reader(async () => ({ type: "ready", page: bad })), () => { })
  await controller.open("session")
  expect(controller.snapshot.error).toContain("u64")
  expect(controller.cache.usage.entries).toBe(0)
  controller.dispose()
})

test("invalidated rows stay byte-owned until the mounted consumer accepts their replacement", async () => {
  let updated = false
  let inspectHandoff = false
  const cache = new ClientCache<HistoryCacheValue>()
  const controller = new HistoryController(reader(async session => {
    const result = page(session, 2, updated ? "new" : "old", "0", updated ? "1001" : "1000")
    if (updated) result.invalidation = { type: "items", items: ["2"] }
    return { type: "ready", page: result }
  }), () => {
    if (inspectHandoff && !controller.snapshot.loading) {
      expect(cache.usage.pinnedEntries).toBe(2)
      expect(controller.snapshot.page?.items[0]?.revision).toBe("1001")
    }
  }, cache)
  await controller.open("session")
  updated = inspectHandoff = true
  await controller.refresh()
  expect(cache.usage.pinnedEntries).toBe(1)
  expect(cache.usage.entries).toBe(1)
  controller.dispose()
})

test("session revisits and refreshes resolve the visible stable anchor after eviction", async () => {
  const requested: { session: string; position: unknown }[] = []
  const cache = new ClientCache<HistoryCacheValue>({ bytes: 16_000, entries: 2 })
  const controller = new HistoryController(reader(async (session, request) => {
    requested.push({ session, position: request.position })
    const first = request.position.type === "around" ? Number(request.position.item)
      : request.position.type === "at_ordinal" ? Number(request.position.ordinal) : 999
    return { type: "ready", page: page(session, first) }
  }), () => { }, cache)
  await controller.open("parent")
  await controller.seek(400n)
  controller.setAnchor({ id: "400", offset: -2 })
  await controller.open("child")
  await controller.seek(600n)
  await controller.open("parent")
  expect(requested.at(-1)).toEqual({ session: "parent", position: { type: "around", item: "400" } })
  expect(controller.snapshot.following).toBe(false)
  expect(controller.snapshot.anchor).toEqual({ id: "400", offset: -2 })
  await controller.refresh()
  expect(requested.at(-1)).toEqual({ session: "parent", position: { type: "around", item: "400" } })
  controller.dispose()
})
