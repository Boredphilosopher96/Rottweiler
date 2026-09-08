import type { RottweilerApp } from "../app"
import { TRANSCRIPT_PROJECTION_VERSION, type TranscriptItem, type TranscriptPage, type TranscriptRead } from "../protocol"
import type { MemoryFixture } from "./memory-fixture"
import type { createMemoryRenderer } from "./memory-renderer"

/** Pure indexed fixture: each request materializes only its bounded mixed page. */
export function mixedHistoryPage(session: string, read: TranscriptRead, total: number, through: string): TranscriptPage {
  const count = Math.min(read.max_items, 32, total), position = read.position
  const selected = "item" in position ? Number(position.item) : null
  const requested = position.type === "latest" ? total - count : position.type === "at_ordinal" ? Number(position.ordinal)
    : position.type === "around" ? selected! - Math.floor(count / 2)
    : position.type === "before" ? selected! - count : position.type === "after" ? selected! + 1 : 0
  const first = Math.max(0, Math.min(total - count, requested))
  return {
    view: { session_id: session, generation: "0", through, projection_version: TRANSCRIPT_PROJECTION_VERSION, digest: Array(32).fill(0) as TranscriptPage["view"]["digest"] },
    first_ordinal: String(first), total_items: String(total), invalidation: { type: "none" },
    anchor: selected === null ? { type: "unspecified" } : { type: "exact", item: String(selected) },
    items: Array.from({ length: count }, (_, offset) => mixedItem(first + offset)),
  }
}
function mixedItem(ordinal: number): TranscriptItem {
  const id = String(ordinal), source = { sequence: id, selector: { type: "conversation" as const } }
  const text = `Historical item ${id}\n` + "bounded historical content ".repeat(128)
  const base = { id, ordinal: id, revision: id, agent_turn: id }
  if (ordinal % 3 === 0) return { ...base, content: { type: "conversation", role: "user", omitted_blocks: false, source,
    blocks: [{ type: "text", body: { text, format: "text", complete: true, source } }] } }
  if (ordinal % 3 === 1) return { ...base, content: { type: "conversation", role: "assistant", omitted_blocks: false, source,
    blocks: [{ type: "text", body: { text: `# Result ${id}\n\n\`\`\`typescript\nconst historicalItem = ${id}\n\`\`\`\n` + text, format: "text", complete: true, source } }] } }
  return { ...base, content: { type: "tool", invocation_id: `historical-${id}`, name: "read", call_index: 0,
    arguments: { text: `{"path":"file-${id}.ts"}`, format: "json", complete: true, source: { sequence: id, selector: { type: "tool_arguments" } } }, diff: null,
    status: { type: "finished", is_error: false, output: { text, format: "text", complete: true, source: { sequence: id, selector: { type: "tool_output" } } }, presentation: null } } }
}

export async function exerciseHistory(app: RottweilerApp, fixture: MemoryFixture,
  setup: Awaited<ReturnType<typeof createMemoryRenderer>>["setup"]) {
  const observations: { stage: string; anchor: string | null; mounted: number; cacheBytes: number }[] = []
  const render = async () => { await setup.renderOnce(); await setup.flush() }
  const requireThat = (value: unknown, message: string) => { if (!value) throw new Error(message) }
  const record = (stage: string) => {
    const mounted = app.transcript.mountedEntryCount
    requireThat(mounted > 0 && mounted <= 16, "historical viewport exceeded its mounted window")
    requireThat(app.historyCache.usage.bytes <= app.historyCache.capacityBytes, "history cache exceeded byte capacity")
    observations.push({ stage, anchor: app.transcript.captureHistoryViewport()?.anchor?.id ?? null, mounted, cacheBytes: app.historyCache.usage.bytes })
  }
  const reveal = async (id: string, stage: string) => {
    const selected = await app.transcript.revealHistorySource(id)
    await render()
    requireThat(selected?.type === "exact" && selected.item === id && app.transcript.mountedCards.has(id), `historical source ${id} is unreachable during ${stage}: ${JSON.stringify({ selected, mounted: app.transcript.mountedKeys })}`)
    requireThat(app.transcript.captureHistoryViewport()?.anchor?.id === id, `historical source ${id} is not the visible anchor`)
    record(stage)
  }
  await reveal("0", "earliest")
  await reveal("5000", "middle")
  const before = app.transcript.captureHistoryViewport()
  fixture.appendHistory()
  app.handleEvent(fixture.historyReady())
  await render()
  requireThat(JSON.stringify(app.transcript.captureHistoryViewport()) === JSON.stringify(before), "append while scrolled away moved the anchor")
  record("append-away")
  setup.resize(72, 30); await render()
  requireThat(JSON.stringify(app.transcript.captureHistoryViewport()) === JSON.stringify(before), `resize changed the stable source/offset anchor: ${JSON.stringify({ before, after: app.transcript.captureHistoryViewport() })}`)
  record("resize")
  setup.resize(110, 36); await render()
  await reveal(String(fixture.historyRows - 1), "latest")
  app.historyCache.clear()
  const reads = fixture.historyReads
  app.setState({ ...app.state, connection: { ...app.state.connection, phase: "reconnecting" } })
  app.setState({ ...app.state, connection: { ...app.state.connection, phase: "connected" } })
  app.handleEvent(fixture.historyReady())
  await reveal("5000", "evicted-middle-after-reconnect")
  requireThat(fixture.historyReads > reads, "evicted history was not fetched through the source reader")
  await reveal(String(fixture.historyRows - 1), "latest-after-reconnect")
  return { initialRows: 10_000, finalRows: fixture.historyRows, mixedKinds: ["user", "assistant-markdown-code", "tool"], observations }
}
