import type { RottweilerApp } from "../app"
import { TRANSCRIPT_PROJECTION_VERSION, type TranscriptItem, type TranscriptPage, type TranscriptRead } from "../protocol"
import type { MemoryFixture } from "./memory-fixture"
import type { createMemoryRenderer } from "./memory-renderer"

/** Pure indexed fixture: each request materializes only its bounded mixed page. */
export function mixedHistoryPage(session: string, read: TranscriptRead, total: number, through: string): TranscriptPage {
  const count = Math.min(read.max_items, 32, total), position = read.position
  if (position.type === "search_match" && (position.source.session_id !== session || position.source.source_sequence !== "15000"
    || position.source.through !== through || position.source.digest.some(byte => byte !== 0))) throw new Error("fixture search token differs")
  const selected = position.type === "search_match" ? 5001 : "item" in position ? Number(position.item) : null
  const requested = position.type === "latest" ? total - count : position.type === "at_ordinal" ? Number(position.ordinal)
    : position.type === "around" || position.type === "search_match" ? selected! - Math.floor(count / 2)
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
  const observations: { stage: string; anchor: string | null; through: string | null; mounted: number; cacheBytes: number }[] = []
  const render = async () => { await setup.renderOnce(); await setup.flush() }
  const requireThat = (value: unknown, message: string) => { if (!value) throw new Error(message) }
  const record = (stage: string) => {
    const mounted = app.transcript.mountedEntryCount
    requireThat(mounted > 0 && mounted <= 16, "historical viewport exceeded its mounted window")
    requireThat(app.historyCache.usage.bytes <= app.historyCache.capacityBytes, "history cache exceeded byte capacity")
    observations.push({ stage, anchor: app.transcript.captureHistoryViewport()?.anchor?.id ?? null, through: app.transcript.historyView?.through ?? null, mounted, cacheBytes: app.historyCache.usage.bytes })
  }
  const reveal = async (id: string, stage: string) => {
    const selected = await app.transcript.revealHistorySource(id)
    await render()
    const deadline = performance.now() + 10_000
    while (app.transcript.captureHistoryViewport() === null) {
      requireThat(performance.now() < deadline, `historical source ${id} did not settle during ${stage}`)
      await Bun.sleep(1); await render()
    }
    requireThat(app.transcript.historyView?.through === fixture.historyThrough, "navigation did not apply its exact source prefix")
    requireThat(selected?.type === "exact" && selected.item === id && app.transcript.mountedCards.has(id), `historical source ${id} is unreachable during ${stage}: ${JSON.stringify({ selected, mounted: app.transcript.mountedKeys })}`)
    requireThat(app.transcript.captureHistoryViewport()?.anchor?.id === id, `historical source ${id} is not the visible anchor during ${stage}: ${JSON.stringify(app.transcript.captureHistoryViewport())}`)
    record(stage)
  }
  await reveal("0", "earliest")
  await reveal("5000", "middle")
  const before = app.transcript.captureHistoryViewport()
  const physicalAnchor = () => {
    const top = app.transcript.scroller.viewport.y
    const bottom = top + app.transcript.scroller.viewport.height
    const visible = [...app.transcript.mountedCards.values()]
      .filter(card => card.visible && card.y + card.height > top && card.y < bottom)
      .sort((left, right) => left.y - right.y)[0]
    return visible === undefined ? null : { id: visible.item.id, offset: visible.y - top }
  }
  fixture.appendHistory()
  app.handleEvent(fixture.historyReady())
  const appendDeadline = performance.now() + 10_000
  for (;;) {
    await render()
    requireThat(JSON.stringify(physicalAnchor()) === JSON.stringify(before?.anchor),
      `append while scrolled away moved the physical anchor: ${JSON.stringify({ before, after: physicalAnchor() })}`)
    // Refresh is coalesced and crosses HTTP: rendering one frame is not read completion.
    // The pending-read viewport remains unavailable to handoff until its source is applied.
    if (app.transcript.historyView?.through === fixture.historyThrough && app.transcript.captureHistoryViewport() !== null) break
    requireThat(performance.now() < appendDeadline, `appended history prefix ${fixture.historyThrough} was not applied`)
    await Bun.sleep(1)
  }
  requireThat(JSON.stringify(app.transcript.captureHistoryViewport()) === JSON.stringify(before),
    `append while scrolled away moved the settled anchor: ${JSON.stringify({ before, after: app.transcript.captureHistoryViewport() })}`)
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
  app.openSessionPicker()
  const deadline = performance.now() + 10_000
  await render()
  while (!app.picker.visible || !app.picker.input.visible || app.picker.input.width === 0
    || setup.renderer.currentFocusedRenderable !== app.picker.input
    || !app.picker.select.options.some(option => option.value === "sessions.new")) {
    if (performance.now() >= deadline) throw new Error("session picker did not become editable")
    await Bun.sleep(1); await render()
  }
  // A remote option can arrive before the editable modal is presented. Send
  // terminal bytes only to the focused input after its native layout has run.
  const inputBefore = { focus: setup.renderer.currentFocusedRenderable?.id, editable: app.picker.input.visible, visible: app.picker.visible, query: app.picker.input.value }
  await setup.mockInput.typeText("needle-in-message")
  const inputAfter = { focus: setup.renderer.currentFocusedRenderable?.id, editable: app.picker.input.visible, visible: app.picker.visible, query: app.picker.input.value }
  while (!app.picker.select.options.some(option => option.value === "memory-probe")) {
    requireThat(app.picker.input.value === "needle-in-message",
      `session search lost its terminal query: ${JSON.stringify({ inputBefore, inputAfter, query: app.picker.input.value })}`)
    if (performance.now() >= deadline) throw new Error(`indexed search result was filtered out of the picker: ${JSON.stringify({ inputBefore, inputAfter, focus: setup.renderer.currentFocusedRenderable?.id, editable: app.picker.input.visible, composer: app.composer.value.slice(0, 100), query: app.picker.input.value, results: app.state.sessionSearch, options: app.picker.select.options.map(option => option.value), errors: app.state.errors.slice(-3) })}`)
    await Bun.sleep(1); await render()
  }
  app.picker.select.setSelectedIndex(app.picker.select.options.findIndex(option => option.value === "memory-probe"))
  app.picker.select.selectCurrent()
  await render()
  const matchIndex = app.picker.select.options.findIndex(option => option.value === "match")
  requireThat(matchIndex >= 0, "source search hit has no exact jump action")
  app.picker.select.setSelectedIndex(matchIndex); app.picker.select.selectCurrent()
  while (app.transcript.captureHistoryViewport()?.anchor?.id !== "5001") {
    if (performance.now() >= deadline) throw new Error("search result did not reveal its exact semantic row")
    await Bun.sleep(1); await render()
  }
  requireThat(app.transcript.mountedCards.has("5001") && !app.transcript.mountedCards.has("15000"), "search source was confused with semantic row identity")
  record("search-match")
  return { initialRows: 10_000, finalRows: fixture.historyRows, mixedKinds: ["user", "assistant-markdown-code", "tool"], observations }
}
