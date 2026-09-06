import type { RottweilerApp } from "../app"
import type { ClientAllocationOwner } from "../client-allocation"
import { PROTOCOL_VERSION } from "../protocol"
import { DocumentController } from "../history/document"
import { MEMORY_LOAD, type MemoryFixture } from "./memory-fixture"

export async function exerciseLiveOwners(app: RottweilerApp, fixture: MemoryFixture, allocations: ClientAllocationOwner,
  render: () => Promise<void>, sample: (stage: string) => void): Promise<void> {
  let sequence = 20000
  const meta = () => ({ protocol_version: PROTOCOL_VERSION, session_id: "memory-probe", sequence_id: String(sequence++), emitted_at: "2026-09-06T00:00:00Z" })
  app.handleEvent({ type: "turn_started", meta: meta(), turn_id: "probe-turn" })
  for (let index = 0; index < MEMORY_LOAD.toolInvocations; index++) {
    const identity = { turn_id: "probe-turn", tool_call_id: `provider-${index}`, invocation_id: `invocation-${index}` }
    app.handleEvent({ type: "tool_call_started", meta: meta(), ...identity, name: "bash", args: { command: "printf output" }, call_index: index })
    for (let chunk = 0; chunk < MEMORY_LOAD.toolChunks; chunk++) {
      app.handleEvent({ type: "tool_output_delta", meta: meta(), ...identity, stream: "stdout", chunk: `${index}:${chunk} ${"stream output ".repeat(1260)}`.slice(0, MEMORY_LOAD.toolChunkBytes) })
    }
  }
  await render()
  if (Object.keys(app.state.tools).length !== MEMORY_LOAD.toolInvocations) throw new Error("live tools were not retained")
  const controlsLease = allocations.reserve("controls", 1024)
  try {
    const reply = await fixture.client.postCommand({ type: "get_session_controls", meta: fixture.meta(), session_id: "memory-probe" }, undefined, controlsLease)
    if (reply.type !== "read" || reply.events[0]?.type !== "session_controls_ready") throw new Error("missing control snapshot")
    app.handleEvent(reply.events[0])
    sample("incoming-controls-with-live-tools")
  } finally { controlsLease.release() }
  await render()
  if (Object.keys(app.state.questions).length !== MEMORY_LOAD.questions) throw new Error("pending questions were dropped")
  sample("mounted-controls-live-tool-previews")
  if (app.recycleState() !== null) throw new Error("unsettled interaction unexpectedly allowed recycle")

  const document = new DocumentController(fixture.reader, app.historyCache, snapshot => {
    if (snapshot.open) app.outputViewer.showDocument(snapshot)
    else app.outputViewer.closePresentation()
  })
  try {
    await document.openSource({ sessionId: "memory-probe", scope: { type: "session" } }, { sequence: "19999", selector: { type: "command_message" } })
    if (document.snapshot.page === null) throw new Error(`canonical document failed: ${document.snapshot.error}`)
    await render(); sample("viewer-owned-canonical-page")
    await document.next()
    if (document.snapshot.page?.offset === 0) throw new Error("document did not advance by source page")
    fixture.hold()
    const stale = document.next()
    const deadline = performance.now() + 10_000
    while (fixture.pending === 0) {
      if (performance.now() >= deadline) throw new Error("document read did not start")
      await Bun.sleep(1)
    }
    document.close()
    fixture.release(); await stale
    if (document.snapshot.open || app.outputViewer.visible) throw new Error("stale document reply reopened a closed viewer")
    sample("stale-viewer-read-settled")
  } finally { fixture.release(); document.close(); app.outputViewer.closePresentation() }
  // Source-confirmed fixture decisions close controls before the explicit process handoff.
  for (let index = 0; index < MEMORY_LOAD.questions; index++) {
    app.handleEvent({ type: "question_answered", meta: { protocol_version: PROTOCOL_VERSION, session_id: "memory-probe", sequence_id: String(20001 + MEMORY_LOAD.toolInvocations * (1 + MEMORY_LOAD.toolChunks) + index), emitted_at: "2026-09-06T00:00:01Z" },
      turn_id: "probe-turn", question_id: `question-${index}`, answers: [] })
  }
  await render()
  if (app.interactionPanel.visible || Object.keys(app.state.questions).length !== 0) throw new Error("settled controls remain interactive")
}
