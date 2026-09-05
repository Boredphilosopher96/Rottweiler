import { ClientDiagnostics } from "../../src/client-diagnostics"
import { expect, test } from "bun:test"
import { createTestRenderer, MockTreeSitterClient } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import { PROTOCOL_VERSION, type CommandReply, type EngineEvent } from "../../src/protocol"
import { TuiEngineRuntime } from "../../src/runtime"
import { EngineHttpSseClient } from "../../src/transport"
import { conversationItem, historyReaderFor, toolItem, waitForHistory } from "../fixtures/history"
import { AuthenticatedMockEngine, encodeSseJson, splitBytes } from "../support/mock-engine"

const SESSION_ID = "session-m9-replay-golden"

test("authenticated observer reads semantic pages without replaying lifetime events", async () => {
  const ready = {
    type: "session_history_ready",
    meta: { protocol_version: PROTOCOL_VERSION, client_id: "minted-client", request_id: "history-ready", emitted_at: "2026-01-01T00:00:10Z" },
    session_id: SESSION_ID, through_sequence: "8",
  } satisfies EngineEvent
  const diagnostics = new ClientDiagnostics()
  const source = historyReaderFor([
    conversationItem(2, "user", "Inspect the persisted project plan."),
    toolItem(4, "read", '{"path":"PROJECT.md"}', "The canonical project plan."),
    conversationItem(7, "assistant", "## Persisted replay verified\n\nRead through the authenticated observer channel."),
  ])
  const engine = new AuthenticatedMockEngine([
    { chunks: splitBytes(encodeSseJson(ready), [1, 7, 31, 2, 127, 5]), holdOpen: true },
  ], async command => {
    if (command.type !== "read_transcript") return { type: "command", outcome: { type: "accepted" } }
    return { type: "read", outcome: { type: "accepted" }, events: [{ type: "transcript_page_ready",
      meta: { ...command.meta, emitted_at: "2026-01-01T00:00:10Z" }, session_id: command.session_id,
      result: await source.page(command.session_id, command.read, new AbortController().signal),
    }] } satisfies CommandReply
  })
  await engine.start()
  const setup = await createTestRenderer({ width: 112, height: 32, useThread: false })
  const treeSitter = new MockTreeSitterClient({ autoResolveTimeout: 0 })
  treeSitter.setMockResult({ highlights: [] })
  let nextRequest = 0
  const runtime = new TuiEngineRuntime({ socketPath: engine.socketPath, bootstrapToken: engine.bootstrapToken,
    sessionId: SESSION_ID, lastSeenSequence: null, lastSeenFile: null, replayMode: true,
  }, new EngineHttpSseClient({ socketPath: engine.socketPath, bootstrapToken: engine.bootstrapToken, diagnostics }), undefined,
  () => `m9-runtime-${nextRequest++}`)
  const app = createRottweilerApp(setup.renderer, { historyReader: runtime.historyReader, diagnostics,
    sessionId: SESSION_ID, replaySessionId: SESSION_ID, treeSitterClient: treeSitter })
  setup.renderer.root.add(app)
  runtime.bind(app)
  const running = runtime.start()
  try {
    await waitForHistory(setup, () => app.state.historyReady !== null && app.transcript.mountedEntryCount === 3 && !treeSitter.isHighlighting())
    expect(engine.commands).toContainEqual(expect.objectContaining({ type: "attach_session", session_id: SESSION_ID, role: "observer", last_seen_sequence: null }))
    expect(engine.commands.filter(command => command.type === "read_transcript")).not.toHaveLength(0)
    for (const command of engine.commands) if (command.type === "read_transcript") {
      expect(command.read.max_items).toBeLessThanOrEqual(32)
      expect(command.read.max_bytes).toBeLessThanOrEqual(256 * 1024)
    }
    expect(app.state.lastSequence).toBeNull()
    expect(app.state.replay.completedThrough).toBeNull()
    expect(app.state.transcript).toHaveLength(0)
    expect(app.state.protocol.invalidEvents).toBe(0)
    const frame = setup.captureCharFrame()
    expect(frame).toContain("Persisted replay verified")
    expect(frame).toContain("read · done")
    app.transcript.selectNextBlock()
    app.transcript.toggleSelectedBlock()
    await waitForHistory(setup, () => !treeSitter.isHighlighting())
    expect(setup.captureCharFrame()).toContain("The canonical project plan.")
    const stages = diagnostics.snapshot().stages
    for (const stage of ["event_decode", "reply_decode", "reply_validation", "reducer", "presentation", "history_admission", "history_update", "history_layout"] as const) {
      expect(stages.find(value => value.stage === stage)?.count).toBeGreaterThan(0)
    }
    expect(JSON.stringify(stages)).not.toContain("PROJECT.md")
  } finally {
    await runtime.stop()
    await running
    setup.renderer.destroy()
    await treeSitter.destroy()
    await engine.stop()
  }
})
