import { describe, expect, test } from "bun:test"
import { ClientCache } from "../src/history/cache"
import { retainedJsonBytes } from "../src/retained-json"

describe("aggregate client cache", () => {
  test("parent, child, artifact and body entries compete for one allowance", () => {
    const cache = new ClientCache<{ text: string }>({ bytes: 2000, entries: 3 })
    for (const key of ["parent:1", "child:1", "artifact:1", "body:1"]) {
      expect(cache.insert(key, { text: key.repeat(20) })).toBe(true)
      expect(cache.usage.bytes).toBeLessThanOrEqual(2000)
      expect(cache.usage.entries).toBeLessThanOrEqual(3)
    }
    expect(cache.lease("parent:1")).toBeNull()
    const body = cache.lease("body:1")
    expect(body?.value.text).toBe("body:1".repeat(20))
    body?.release()
  })

  test("a mounted old revision stays correct and charged across replacement", () => {
    const cache = new ClientCache<{ text: string }>({ bytes: 2000, entries: 2 })
    cache.insert("item", { text: "old" })
    const old = cache.lease("item")!
    const oldBytes = cache.usage.bytes
    expect(cache.insert("item", { text: "new" })).toBe(true)
    const current = cache.lease("item")!
    expect(old.value.text).toBe("old")
    expect(current.value.text).toBe("new")
    expect(cache.usage.entries).toBe(2)
    expect(cache.usage.bytes).toBeGreaterThan(oldBytes)
    expect(cache.insert("third", { text: "denied" })).toBe(false)
    old.release()
    expect(() => old.value).toThrow("released")
    old.release()
    expect(cache.usage.entries).toBe(1)
    current.release()
    cache.clear()
    expect(cache.usage).toEqual({ bytes: 0, entries: 0, residentEntries: 0, pinnedEntries: 0 })
  })

  test("failed admission is atomic and invalidation does not release mounted bytes early", () => {
    const cache = new ClientCache<{ text: string }>({ bytes: 900, entries: 2 })
    cache.insert("visible", { text: "selected" })
    cache.insert("other", { text: "cached" })
    const visible = cache.lease("visible")!
    const before = cache.usage
    expect(cache.insert("huge", { text: "x".repeat(900) })).toBe(false)
    expect(cache.usage).toEqual(before)
    cache.clear()
    expect(cache.usage.entries).toBe(1)
    expect(cache.usage.residentEntries).toBe(0)
    expect(visible.value.text).toBe("selected")
    visible.release()
    expect(cache.usage.bytes).toBe(0)
  })

  test("revisiting thousands of evicted pages plateaus allocation ownership", () => {
    const cache = new ClientCache<{ text: string }>({ bytes: 2048, entries: 8 })
    for (let index = 0; index < 10_000; index++) {
      const key = `page:${index % 300}`
      expect(cache.insert(key, { text: `payload-${index}` })).toBe(true)
      const lease = cache.lease(key)!
      expect(lease.value.text).toBe(`payload-${index}`)
      lease.release()
      expect(cache.usage.bytes).toBeLessThanOrEqual(2048)
      expect(cache.usage.entries).toBeLessThanOrEqual(8)
    }
  })

  test("decoding reservations compete with mounted history and survive invalidation until settlement", () => {
    const cache = new ClientCache<{ text: string }>({ bytes: 2000, entries: 2 })
    cache.insert("history", { text: "visible" })
    const history = cache.lease("history")!
    const reserved = cache.reserve(1200)!
    expect(cache.reserve(1200)).toBeNull()
    const pendingBytes = cache.usage.bytes
    cache.clear()
    expect(cache.usage.bytes).toBe(pendingBytes)
    const surface = reserved.commit("surface", { text: "decoded" })
    expect(cache.usage.bytes).toBeLessThan(pendingBytes)
    reserved.release()
    expect(surface.value.text).toBe("decoded")
    history.release()
    surface.release()
    cache.clear()
    expect(cache.usage.bytes).toBe(0)
  })

  test("failed decoded admission keeps its reservation charged until cleanup", () => {
    const cache = new ClientCache<{ text: string }>({ bytes: 1000, entries: 2 })
    const reserved = cache.reserve(500)!
    expect(() => reserved.commit("large", { text: "x".repeat(1000) })).toThrow("reserved")
    expect(cache.usage.bytes).toBe(500)
    reserved.release()
    reserved.release()
    expect(cache.usage.bytes).toBe(0)
    expect(() => reserved.commit("late", { text: "x" })).toThrow("released")
  })

  test("measurement rejects excessive nesting and non-JSON ownership", () => {
    const cycle: { next?: unknown } = {}
    cycle.next = cycle
    expect(retainedJsonBytes(cycle, 100_000)).toBeGreaterThan(100_000)
    expect(retainedJsonBytes({ renderer: () => { } }, 1000)).toBeGreaterThan(1000)
    expect(retainedJsonBytes("🐕", 1000)).toBe(28)
  })
})
