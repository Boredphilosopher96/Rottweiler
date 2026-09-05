import { expect, test } from "bun:test"
import { ComposerDraftStore } from "../src/composer-drafts"
import { ClientCache } from "../src/history/cache"
import { HistoryController, type HistoryCacheValue } from "../src/history/controller"
import { TimelineController, readTimelineDraft } from "../src/history/timeline"
import { conversationItem, sessionReaderFor } from "./fixtures/history"

function fixture(text: string) {
  const item = conversationItem(9, "user", "preview only")
  const pages = sessionReaderFor([item])
  const calls: number[] = []
  const bytes = Buffer.from(text)
  const reader = { ...pages, content: async (_session: string, read: import("../src/protocol").TranscriptContentRead) => {
    calls.push(read.offset)
    const chunk = bytes.subarray(read.offset, read.offset + read.max_bytes).toString()
    const end = read.offset + Buffer.byteLength(chunk)
    return { view: read.view, source: read.source, offset: read.offset, total_bytes: bytes.length,
      next_offset: end < bytes.length ? end : null, text: chunk, format: "text" as const }
  } }
  return { reader, calls }
}

test("timeline pages restore sources beyond the recent live window using the shared byte owner", async () => {
  const cache = new ClientCache<HistoryCacheValue>()
  const reader = sessionReaderFor(Array.from({ length: 400 }, (_, index) => conversationItem(index, "user", `request ${index}`)))
  const mounted = new HistoryController(reader, () => {}, cache)
  const timeline = new TimelineController(reader, cache, () => {})
  await mounted.open("s")
  const mountedPage = mounted.snapshot.page
  await timeline.open("s")
  expect(timeline.choices[0]?.sequenceId).toBe("399")
  for (let page = 0; page < 10; page++) await timeline.previous()
  expect(timeline.choices[0]?.sequenceId).toBe("79")
  expect(timeline.choices).toHaveLength(32)
  expect(timeline.newer).toBe(true)
  expect(mounted.snapshot.page).toBe(mountedPage)
  timeline.dispose()
  expect(mounted.snapshot.page).toBe(mountedPage)
  await mounted.seek(380n)
  expect(mounted.snapshot.page).toBe(mountedPage)
  mounted.dispose()
  expect(cache.usage.bytes).toBe(0)
})

test("source reads reserve all text before continuation and merge without discarding a concurrent draft", async () => {
  const text = "exact full body\n" + "x".repeat(10000)
  const { reader, calls } = fixture(text)
  const timeline = new TimelineController(reader, new ClientCache(), () => {})
  await timeline.open("s")
  const drafts = new ComposerDraftStore()
  drafts.set("parent", { content: "new draft", attachments: [] })
  const pending = await readTimelineDraft(reader, timeline.choices[0]!, drafts, "parent", new AbortController().signal)
  expect(pending.draft.content).toBe(text)
  expect(calls).toEqual([0, 4096, 8192])
  expect(drafts.usage.pending).toBe(1)
  expect(drafts.replace([])).toBe(false)
  expect(pending.settle(false)?.content).toBe(`${text}\nnew draft`)
  expect(drafts.usage.pending).toBe(0)
  timeline.dispose()
})

test("draft capacity rejection prevents the next content allocation and preserves existing text", async () => {
  const { reader, calls } = fixture("x".repeat(10000))
  const timeline = new TimelineController(reader, new ClientCache(), () => {})
  await timeline.open("s")
  const drafts = new ComposerDraftStore(2000)
  drafts.set("parent", { content: "keep", attachments: [] })
  await expect(readTimelineDraft(reader, timeline.choices[0]!, drafts, "parent", new AbortController().signal)).rejects.toThrow("Draft capacity")
  expect(calls).toEqual([0])
  expect(drafts.get("parent").content).toBe("keep")
  expect(drafts.usage.pending).toBe(0)
  timeline.dispose()
})

test("source mismatch, stalled chunks, and cancellation release read reservations", async () => {
  for (const failure of ["source", "offset", "abort"] as const) {
    const { reader } = fixture("x".repeat(10000))
    const timeline = new TimelineController(reader, new ClientCache(), () => {})
    await timeline.open("s")
    const drafts = new ComposerDraftStore()
    const abort = new AbortController()
    const broken = { ...reader, content: async (session: string, read: import("../src/protocol").TranscriptContentRead) => {
      const page = await reader.content(session, read)
      if (read.offset > 0) {
        if (failure === "abort") abort.abort()
        if (failure === "source") return { ...page, source: { ...page.source, sequence: "999" } }
        if (failure === "offset") return { ...page, next_offset: read.offset }
      }
      return page
    } }
    await expect(readTimelineDraft(broken, timeline.choices[0]!, drafts, "parent", abort.signal)).rejects.toThrow()
    expect(drafts.usage.bytes).toBe(0)
    expect(drafts.usage.pending).toBe(0)
    timeline.dispose()
  }
})

test("retiring a session while source content is read prevents later draft resurrection", () => {
  const drafts = new ComposerDraftStore()
  const read = drafts.reserveText("parent", 100)!
  const charge = drafts.usage.bytes
  drafts.clear()
  expect(drafts.usage.bytes).toBe(charge)
  const pending = read.finish("old session")
  expect(pending.settle(false)).toBeNull()
  expect(drafts.usage.bytes).toBe(0)
})
