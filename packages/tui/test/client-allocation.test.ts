import { expect, test } from "bun:test"
import { ClientAllocationOwner, type ClientAllocationDomain } from "../src/client-allocation"
import { ClientCache } from "../src/history/cache"
const limits: Record<ClientAllocationDomain, number> = { history: 4096, controls: 4096, metadata: 4096, children: 4096, tasks: 4096 }

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
  const claim = owner.reserve("metadata", 2000 - owner.usage.bytes)
  expect(cache.insert("body", { text: "replacement".repeat(30) })).toBe(false)
  const original = cache.lease("body")!
  expect(original.value).toEqual({ text: "original" })
  original.release(); cache.clear(); claim.release()
  expect(owner.usage.bytes).toBe(0)
})
