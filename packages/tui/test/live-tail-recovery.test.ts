import { ToolOutputReader } from "../src/state/output-reader"
import { expect, test } from "bun:test"
import { ClientCache } from "../src/history/cache"
import type { HistoryCacheValue } from "../src/history/controller"
import { collectLiveTail, type TailRead } from "../src/history/live-tail"
import type { EngineEvent, TranscriptTailContent, TranscriptTailIdentity, TranscriptTailPage, TranscriptTailPart } from "../src/protocol"
import { PROTOCOL_VERSION, TRANSCRIPT_PROJECTION_VERSION } from "../src/protocol"
import { directSessionRead } from "../src/session-reader"
import { createInitialState, engineEvent, reduceRottweilerState } from "../src/state"
import { installLiveTail } from "../src/state/tail-recovery"
import { ToolOutputBuffer } from "../src/state/display-buffer"

const identity: TranscriptTailIdentity = { generation: "1", turn_started: "5", response_epoch: "5", tools_epoch: "5" }
function page(content: TranscriptTailContent, through = "10", epoch = identity): TranscriptTailPage {
  return { identity: epoch, content, view: { session_id: "s", projection_version: TRANSCRIPT_PROJECTION_VERSION,
    generation: epoch.generation, through, digest: Array(32).fill(0) as TranscriptTailPage["view"]["digest"] } }
}
function empty(part: TranscriptTailPart): TranscriptTailContent {
  return part.type === "text" || part.type === "thinking" ? { type: part.type, preview: { text: "", truncated: false } }
    : { type: part.type, offset: part.offset, items: [], next_offset: null }
}
const target = directSessionRead("s")

test("tail component collection retains one shared cache charge through its final consumer", async () => {
  const cache = new ClientCache<HistoryCacheValue>()
  let concurrent = 0, peak = 0
  const read: TailRead = async (_target, request, _signal, allocation) => {
    concurrent++; peak = Math.max(peak, concurrent)
    allocation.admit(8192)
    await Promise.resolve()
    concurrent--
    return { type: "ready", page: page(request.part.type === "text" ? { type: "text", preview: { text: "live", truncated: false } } : empty(request.part)) }
  }
  const snapshot = await collectLiveTail(read, cache, target, new AbortController().signal)
  expect(peak).toBe(1)
  expect(snapshot.pages).toHaveLength(4)
  expect(cache.usage.pinnedEntries).toBe(4)
  expect(cache.usage.residentEntries).toBe(0)
  cache.clear()
  expect(cache.usage.bytes).toBeGreaterThan(0)
  snapshot.release()
  expect(cache.usage.bytes).toBe(0)
  expect(() => snapshot.pages).toThrow("released")
})

test("changed source epochs and non-progressing pages release every prior component", async () => {
  for (const malformed of [false, true]) {
    const cache = new ClientCache<HistoryCacheValue>()
    const read: TailRead = async (_target, request) => {
      if (request.part.type === "citations") return malformed
        ? { type: "ready", page: page({ type: "citations", offset: 0, items: [], next_offset: 0 }) }
        : { type: "changed", view: page(empty(request.part)).view, identity: { ...identity, response_epoch: "11" } }
      return { type: "ready", page: page(empty(request.part)) }
    }
    await expect(collectLiveTail(read, cache, target, new AbortController().signal)).rejects.toThrow(malformed ? "non-progressing" : "changed")
    expect(cache.usage.bytes).toBe(0)
  }
})

test("cancellation keeps the in-flight allocation until its reader actually settles", async () => {
  const cache = new ClientCache<HistoryCacheValue>()
  const controller = new AbortController()
  let finish!: () => void
  const pending = new Promise<void>(resolve => { finish = resolve })
  const read: TailRead = async (_target, request, _signal, allocation) => {
    allocation.admit(4096)
    await pending
    return { type: "ready", page: page(empty(request.part)) }
  }
  const work = collectLiveTail(read, cache, target, controller.signal)
  controller.abort()
  expect(cache.usage.bytes).toBe(4096)
  finish()
  await expect(work).rejects.toThrow()
  expect(cache.usage.bytes).toBe(0)
})

test("installed components suppress only their covered deltas and preserve explicit preview omission", () => {
  const initial = createInitialState()
  const seeded = { ...initial, recovery: { ...initial.recovery, activeTurnSource: "5" }, turns: {
    "2": { turnId: "2", status: "running" as const, usage: null, cost: null, timing: { kind: "unknown" as const } },
  } }
  let state = installLiveTail(seeded, [
    page({ type: "text", preview: { text: "prefix", truncated: true } }, "10"),
    page({ type: "thinking", preview: { text: "thought", truncated: false } }, "11"),
    page({ type: "citations", offset: 0, items: [{ source: "9", uri: "https://example.com", title: null }], next_offset: null }, "12"),
    page({ type: "tools", offset: 0, items: [], next_offset: null }, "13"),
  ])
  const meta = (sequence: string) => ({ protocol_version: PROTOCOL_VERSION, session_id: "s", sequence_id: sequence, emitted_at: "2026-01-01T00:00:00Z" })
  const apply = (event: EngineEvent) => { state = reduceRottweilerState(state, engineEvent(event), "s") }
  apply({ type: "text_delta", meta: meta("10"), turn_id: "2", text: "already captured" })
  apply({ type: "thinking_delta", meta: meta("11"), turn_id: "2", text: "already captured" })
  apply({ type: "citation_delta", meta: meta("12"), turn_id: "2", uri: "https://example.com", title: null })
  apply({ type: "text_delta", meta: meta("13"), turn_id: "2", text: "cannot fill an omitted gap" })
  apply({ type: "thinking_delta", meta: meta("14"), turn_id: "2", text: " continues" })
  expect(state.lastSequence).toBe("14")
  expect(state.streamingTail?.text).toBe("prefix")
  expect(state.streamingTail?.thinking).toBe("thought continues")
  expect(state.streamingTail?.citations).toHaveLength(1)
  expect(state.streamingTail?.displayBudget.text.omittedBytes).toBeGreaterThan(0)
  const buffer = ToolOutputBuffer.fromPreview("tool prefix", true).append({ stream: "stdout", chunk: "after omitted gap" })
  expect(new ToolOutputReader().read(buffer).plain).toContain("tool prefix")
  expect(new ToolOutputReader().read(buffer).plain).not.toContain("after omitted gap")
})
