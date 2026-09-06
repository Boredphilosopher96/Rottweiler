import { expect, test } from "bun:test"
import { type ClientCommand, type CommandReply, type SessionStateSnapshot } from "../../src/protocol"
import { TuiEngineRuntime } from "../../src/runtime"
import { MemoryFiles, SwitchingClient, TestApp, waitFor } from "./fixtures"

test("the production runtime refreshes a missed compaction revision without replaying the session", async () => {
  const client = new SwitchingClient(), post = client.postCommand.bind(client)
  let reads = 0, revision = "5", text = "captured"
  let refreshed!: () => void
  const refreshDelivered = new Promise<void>(resolve => { refreshed = resolve })
  client.postCommand = async (command: ClientCommand): Promise<CommandReply> => {
    if (command.type !== "get_session_state") return post(command)
    reads++
    const snapshot: SessionStateSnapshot = { through: "1", driver_client_id: command.meta.client_id, title: null, model_alias: "main", provider: null,
      thinking: "off", mode_id: "execute", active_turn: { turn_id: "summary", started: null }, completed_turns: "0", shell: null,
      compaction: { started: "1", revision, summary_turn_id: "summary", attempt: 0, text: { text, truncated: false }, thinking: { text: "", truncated: false } },
      queued_messages: [], budget: null }
    if (reads === 2) refreshed()
    return { type: "read", outcome: { type: "accepted" }, events: [{ type: "session_state_ready", session_id: command.session_id,
      meta: { ...command.meta, emitted_at: "2026-01-01T00:00:00Z" }, snapshot }] }
  }
  const runtime = new TuiEngineRuntime({ socketPath: "/private/engine.sock", bootstrapToken: "secret", sessionId: "s", lastSeenSequence: null, lastSeenFile: null, replayMode: false }, client, new MemoryFiles())
  const app = new TestApp(); runtime.bind(app)
  const running = runtime.start()
  try {
    await waitFor(() => app.state.compaction.text === "captured")
    expect(app.state.lastSequence).toBeNull()
    revision = "7"; text = "captured missing suffix"
    await client.subscriptions[0]!.onEvent({ type: "compaction_text_delta", session_id: "s", started: "1", revision, summary_turn_id: "summary", attempt: 0, text: "suffix" })
    expect(app.state.compaction.text).toBe("captured")
    expect(app.state.recovery.compaction?.stale).toBe(true)
    await refreshDelivered
    await waitFor(() => app.state.compaction.text === text)
    expect(reads).toBe(2)
    expect(app.state.recovery.compaction?.stale).toBe(false)
    expect(client.subscriptions).toHaveLength(1)
    expect(app.state.lastSequence).toBeNull()
  } finally { await runtime.stop(); await running }
})
