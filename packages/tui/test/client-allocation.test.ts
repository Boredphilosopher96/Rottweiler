import { expect, test } from "bun:test"
import { ClientAllocationOwner, type ClientAllocationDomain } from "../src/client-allocation"
import { ClientCache } from "../src/history/cache"
import { ComposerDraftStore } from "../src/composer-drafts"
const limits: Record<ClientAllocationDomain, number> = { outbound: 4096, urgent: 4096, live: 4096, decoding: 4096, history: 4096, drafts: 4096, controls: 4096, metadata: 4096, children: 4096, tasks: 4096 }

test("history and retained snapshots compete for one aggregate allocation owner", () => {
  const owner = new ClientAllocationOwner(limits, 5000)
  const cache = new ClientCache<unknown>({ bytes: 4096, entries: 8 }, owner)
  const mounted = owner.reserve("controls", 2000), incoming = owner.reserve("controls", 2000)
  expect(cache.reserve(2048)).toBeNull()
  expect(owner.usage.bytes).toBe(4000)
  incoming.resize(512)
  const read = cache.reserve(2048)!
  expect(read).not.toBeNull()
  expect(() => incoming.resize(2000)).toThrow("admission")
  expect(incoming.bytes).toBe(512)
  mounted.release()
  incoming.resize(2000)
  const payload = read.commit("body", { text: "mounted" })
  cache.clear()
  expect(owner.usage.domains.history).toBeGreaterThan(0)
  payload.release(); incoming.release()
  expect(owner.usage.bytes).toBe(0)
})

test("allocation release and refused growth preserve exact charges", () => {
  const owner = new ClientAllocationOwner(limits, 4096), first = owner.reserve("metadata", 1024)
  expect(() => first.resize(4097)).toThrow("admission")
  expect(first.bytes).toBe(1024)
  expect(owner.usage.bytes).toBe(1024)
  first.release(); first.release()
  expect(owner.usage.bytes).toBe(0)
  expect(() => first.resize(1)).toThrow("released")
})

test("aggregate refusal preserves the resident revision and unrelated eviction candidates", () => {
  const owner = new ClientAllocationOwner(limits, 2000), cache = new ClientCache<unknown>({ bytes: 4096, entries: 8 }, owner)
  expect(cache.insert("body", { text: "original" })).toBe(true)
  const claim = owner.reserve("metadata", owner.normalCapacity - owner.usage.bytes)
  expect(cache.insert("body", { text: "replacement".repeat(30) })).toBe(false)
  const original = cache.lease("body")!
  expect(original.value).toEqual({ text: "original" })
  original.release(); cache.clear(); claim.release()
  expect(owner.usage.bytes).toBe(0)
})

test("draft edits compete with mounted history before replacing editable data", () => {
  const owner = new ClientAllocationOwner(limits, 2000), drafts = new ComposerDraftStore(4096, 8, owner)
  const history = owner.reserve("history", 1000)
  expect(drafts.set("parent", { content: "keep", attachments: [] })).toBe(true)
  const held = owner.usage.bytes
  expect(drafts.canRetainText("parent", 200, [])).toBe(false)
  expect(drafts.set("parent", { content: "x".repeat(200), attachments: [] })).toBe(false)
  expect(drafts.get("parent").content).toBe("keep")
  expect(owner.usage.bytes).toBe(held)
  history.release()
  expect(drafts.set("parent", { content: "x".repeat(200), attachments: [] })).toBe(true)
  expect(owner.usage.bytes).toBe(drafts.usage.bytes)
  drafts.clear()
  expect(owner.usage.bytes).toBe(0)
})

test("cancelled draft reads and submissions hold aggregate credit until their actual settlement", () => {
  const owner = new ClientAllocationOwner(limits, 4096), drafts = new ComposerDraftStore(4096, 8, owner)
  const read = drafts.reserveDraft("parent", 1024, 0)!
  expect(read).not.toBeNull()
  const before = owner.usage.bytes
  drafts.clear()
  expect(owner.usage.bytes).toBe(before)
  const submission = read.finish({ content: "received", attachments: [] })
  expect(owner.usage.bytes).toBeGreaterThan(0)
  expect(submission.settle(false)).toBeNull()
  expect(owner.usage.bytes).toBe(0)
})

test("draft handoff admits old and incoming data together and transfers its lease atomically", () => {
  const owner = new ClientAllocationOwner(limits, 2000), drafts = new ComposerDraftStore(4096, 8, owner)
  expect(drafts.set("parent", { content: "old", attachments: [] })).toBe(true)
  const old = drafts.usage.bytes, history = owner.reserve("history", owner.normalCapacity - old)
  expect(drafts.replace([{ scope: "parent", draft: { content: "new", attachments: [] } }])).toBe(false)
  expect(owner.usage.bytes).toBe(owner.normalCapacity)
  expect(drafts.get("parent").content).toBe("old")
  history.release()
  expect(drafts.replace([{ scope: "parent", draft: { content: "new", attachments: [] } }])).toBe(true)
  expect(owner.usage.bytes).toBe(old)
  const submission = drafts.submit("parent")!
  expect(submission.draft.content).toBe("new")
  submission.settle(true)
  expect(owner.usage.bytes).toBe(0)
})


test("urgent credit cannot move into full ordinary capacity and refused transfer is atomic", () => {
  const owner = new ClientAllocationOwner(limits, 4096)
  const normal = owner.reserve("outbound", owner.normalCapacity)
  const urgent = owner.reserve("urgent", owner.urgentCapacity)
  expect(owner.usage.bytes).toBe(owner.maximumBytes)
  expect(() => urgent.moveTo("decoding")).toThrow("admission")
  expect(owner.usage.domains.urgent).toBe(owner.urgentCapacity)
  expect(urgent.bytes).toBe(owner.urgentCapacity)
  normal.release()
  urgent.moveTo("decoding")
  expect(owner.usage.domains.urgent).toBe(0)
  expect(owner.usage.domains.decoding).toBe(owner.urgentCapacity)
  urgent.release()
  expect(owner.usage.bytes).toBe(0)
})
