import { directSessionRead } from "../src/session-reader"
import { expect, test } from "bun:test"
import { ClientCache } from "../src/history/cache"
import type { HistoryCacheValue } from "../src/history/controller"
import { DocumentController } from "../src/history/document"
import { TRANSCRIPT_PROJECTION_VERSION, type TranscriptView, type TranscriptContentSource } from "../src/protocol"
import { fixturePresentation, surfacePage } from "./fixtures/ui"

const view: TranscriptView = { session_id: "history", projection_version: TRANSCRIPT_PROJECTION_VERSION, generation: "0", through: "7", digest: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0] }
const source: TranscriptContentSource = { sequence: "7", selector: { type: "tool_presentation", invocation_id: "instance" } }

test("complete tool surfaces collect bounded pages under cache credit and retain prepared field strings", async () => {
  const cache = new ClientCache<HistoryCacheValue>()
  const presentation = fixturePresentation()
  presentation.descriptor.fields = Array.from({ length: 12 }, (_, id) => ({ kind: "text", id: `field-${id}`, label: `Field ${id}` }))
  presentation.projected.fields = Array.from({ length: 12 }, (_, id) => ({ kind: "text", id: `field-${id}`, value: "λ".repeat(2000) }))
  let requests = 0
  const controller = new DocumentController({
    page: async () => { throw new Error("unused") },
    content: async (_session, read) => {
      expect(cache.usage.bytes).toBeGreaterThan(1024 * 1024)
      requests++
      return surfacePage(presentation, read)
    },
  }, cache, () => {})
  await controller.open(directSessionRead(view.session_id), view, source)
  expect(controller.snapshot.error).toBeNull()
  expect(controller.snapshot.surface?.presentation).toEqual(presentation)
  expect(controller.snapshot.surface?.fields[0]?.text).toContain("λ".repeat(2000))
  expect(requests).toBeGreaterThan(1)
  expect(requests).toBeLessThanOrEqual(17)
  const loaded = requests
  controller.close()
  await controller.open(directSessionRead(view.session_id), view, source)
  expect(requests).toBe(loaded)
  expect(cache.usage.pinnedEntries).toBe(1)
  controller.close()
  cache.clear()
  expect(cache.usage.bytes).toBe(0)
})

test("cancelled surface reads hold credit until I/O settles and never publish late data", async () => {
  const cache = new ClientCache<HistoryCacheValue>()
  let finish!: () => void
  const controller = new DocumentController({
    page: async () => { throw new Error("unused") },
    content: async (_session, read) => {
      await new Promise<void>(resolve => { finish = resolve })
      return surfacePage(fixturePresentation(), read)
    },
  }, cache, () => {})
  const pending = controller.open(directSessionRead(view.session_id), view, source)
  controller.close()
  expect(cache.usage.bytes).toBeGreaterThan(0)
  finish()
  await pending
  expect(controller.snapshot.surface).toBeNull()
  expect(cache.usage.bytes).toBe(0)
})

test("surface loading rejects foreign source identity and mismatched fields without retained payload", async () => {
  for (const failure of ["source", "fields", "oversize"] as const) {
    const cache = new ClientCache<HistoryCacheValue>()
    const value = fixturePresentation()
    if (failure === "fields") value.projected.fields.pop()
    if (failure === "oversize") value.projected.fields = [{ kind: "text", id: "summary", value: "x".repeat(65536) }]
    const controller = new DocumentController({
      page: async () => { throw new Error("unused") },
      content: async (_session, read) => {
        const page = surfacePage(value, read)
        return failure === "source" ? { ...page, source: { ...source, sequence: "6" } } : page
      },
    }, cache, () => {})
    await controller.open(directSessionRead(view.session_id), view, source)
    expect(controller.snapshot.error).not.toBeNull()
    expect(controller.snapshot.surface).toBeNull()
    expect(cache.usage.bytes).toBe(0)
    controller.close()
  }
})
