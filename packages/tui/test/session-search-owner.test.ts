import { expect, test } from "bun:test"
import { SessionSearchNavigation } from "../src/app/session-search"
import { ClientCache } from "../src/history/cache"
import type { SessionSearchMatch } from "../src/protocol"

const source: SessionSearchMatch = { session_id: "target", source_sequence: "900", through: "1000", digest: Array(32).fill(0) as SessionSearchMatch["digest"] }

test("selected search token stays charged through session activation and physical read settlement", async () => {
  const cache = new ClientCache<import("../src/history/controller").HistoryCacheValue>()
  const activated = Promise.withResolvers<void>(), settled = Promise.withResolvers<void>()
  let session = "origin", destroyed = false, reads = 0
  const errors: string[] = []
  const navigation = new SessionSearchNavigation({ historyCache: cache,
    get sessionId() { return session }, get destroyed() { return destroyed },
    closePicker() {}, async selectSession(id) { await activated.promise; session = id },
    async navigateTranscript(token) { expect(token).toBe(source); reads++; await settled.promise },
    projectError(code) { errors.push(code) },
  })
  const pending = navigation.open(source)
  try {
    expect(navigation.pending).toBeTrue()
    expect(cache.allocations.usage.bytes).toBeGreaterThan(0)
    await navigation.open(source)
    expect(errors).toEqual(["navigation_pending"])
    activated.resolve(); await Bun.sleep(0)
    expect(reads).toBe(1)
    destroyed = true
    expect(cache.allocations.usage.bytes).toBeGreaterThan(0)
    settled.resolve(); await pending
    expect(cache.allocations.usage.bytes).toBe(0)
    expect(navigation.pending).toBeFalse()
  } finally { activated.resolve(); settled.resolve(); await pending; cache.clear() }
})
