import { expect, test } from "bun:test"
import { ClientCache } from "../src/history/cache"
import { DocumentController } from "../src/history/document"
import type { HistoryCacheValue } from "../src/history/controller"
import { TRANSCRIPT_PROJECTION_VERSION, type TranscriptView, type TranscriptContentSource } from "../src/protocol"

const view: TranscriptView = { session_id: "history", projection_version: TRANSCRIPT_PROJECTION_VERSION, generation: "0", through: "7", digest: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0] }
const source: TranscriptContentSource = { sequence: "7", selector: { type: "command_message" } }

test("document paging restores evicted UTF-8 chunks without assembling full output", async () => {
  const cache = new ClientCache<HistoryCacheValue>({ bytes: 16_000, entries: 3 })
  let requests = 0
  const total = 30 * 4095
  const controller = new DocumentController({
    page: async () => { throw new Error("unused history") },
    content: async (_session, read) => {
      requests++
      return {
        view: read.view, source: read.source, offset: read.offset,
        next_offset: read.offset + 4095 < total ? read.offset + 4095 : null,
        total_bytes: total, format: "text", text: "€".repeat(1365)
      }
    },
  }, cache, () => { })
  await controller.open(view, source)
  for (let index = 0; index < 29; index++) await controller.next()
  expect(requests).toBe(30)
  expect(controller.snapshot.page?.next_offset).toBeNull()
  expect(cache.usage.bytes).toBeLessThanOrEqual(16_000)
  for (let index = 0; index < 29; index++) await controller.previous()
  expect(controller.snapshot.page?.offset).toBe(0)
  expect(requests).toBeGreaterThan(30)
  expect(controller.snapshot.page?.text).toBe("€".repeat(1365))
  controller.close()
  expect(cache.usage.pinnedEntries).toBe(0)
  expect(controller.snapshot.page).toBeNull()
})

test("close aborts an in-flight document and its late reply cannot retain content", async () => {
  const cache = new ClientCache<HistoryCacheValue>()
  let finish!: () => void
  let signal: AbortSignal | undefined
  const controller = new DocumentController({
    page: async () => { throw new Error("unused") },
    content: async (_session, read, requestSignal) => {
      signal = requestSignal
      await new Promise<void>(resolve => { finish = resolve })
      return { view: read.view, source: read.source, offset: 0, next_offset: null, total_bytes: 3, format: "text", text: "old" }
    },
  }, cache, () => { })
  const pending = controller.open(view, source)
  controller.close()
  expect(signal?.aborted).toBe(true)
  finish()
  await pending
  expect(controller.snapshot.open).toBe(false)
  expect(cache.usage.bytes).toBe(0)
})
