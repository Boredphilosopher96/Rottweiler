import { expect, test } from "bun:test"
import { createTestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import type { FamilyControlsReader } from "../../src/family-controls-reader"
import { TRANSCRIPT_PROJECTION_VERSION, type SessionStateSnapshot, type TranscriptTailPage, type ClientCommand } from "../../src/protocol"
import { emptySessionReader, waitForHistory } from "../fixtures/history"

function cancelled(signal: AbortSignal): Promise<never> {
  return new Promise((_, reject) => {
    if (signal.aborted) reject(signal.reason)
    else signal.addEventListener("abort", () => reject(signal.reason), { once: true })
  })
}
const target = { session_id: "child", ancestry: [{ subagent_id: "agent", session_id: "child" }] }
const metadata: SessionStateSnapshot = { through: "10", driver_client_id: "driver", title: "Exact child", model_alias: "fast", provider: null, thinking: "off", mode_id: "execute",
  active_turn: { turn_id: "actual-turn", started: "5" }, completed_turns: "1", shell: null, compaction: null, plugin_statuses: [], queued_messages: [], budget: null }

test("selected child restores metadata, canonical text and pending controls without progress replay", async () => {
  const setup = await createTestRenderer({ width: 110, height: 30, useThread: false })
  const commands: ClientCommand[] = []
  let scopes = 0, stateReads = 0, tailReads = 0, resolved = false
  const reader: FamilyControlsReader = {
    async watch(_root, after, signal) {
      if (after !== null) return cancelled(signal)
      return { revision: "1", children: [{ target, controls: { revision: "1", through: "10", questions: 1, approvals: 0, pending_plan: false, available: true } }] }
    },
    async child() { return { revision: "1", snapshot: { through: "10", controls: { approvals: [], pending_plan: null,
      questions: resolved ? [] : [{ turn_id: "actual-turn", question_id: "question", question: { id: "question", prompt: "Choose the child path", response_kind: "text", options: [] } }] } } } },
    async state(root, selected) { expect(root).toBe("root"); expect(selected).toEqual(target); stateReads++; return metadata },
    async scope(root, selected) { expect(selected).toEqual(target); scopes++; return { type: "ready", scope: { type: "descendant", root_session_id: root,
      ancestry: [{ subagent_id: "agent", session_id: "child", source_sequence: "2" }] } } },
  }
  const app = createRottweilerApp(setup.renderer, { sessionId: "root", familyControls: reader,
    sessionReader: { ...emptySessionReader,
      // Active-child enumeration has no terminal binding; exact live resolution is independent.
      children: async () => { throw new Error("selected history cannot enumerate active children") },
      async tail(selected, request, _signal, allocation) {
        expect(selected).toEqual({ sessionId: "child", scope: { type: "descendant", root_session_id: "root", ancestry: [{ subagent_id: "agent", session_id: "child", source_sequence: "2" }] } })
        allocation.admit(8192); tailReads++
        const part = request.part
        const page: TranscriptTailPage = { view: { session_id: "child", projection_version: TRANSCRIPT_PROJECTION_VERSION, generation: "0", through: "10", digest: Array(32).fill(0) as TranscriptTailPage["view"]["digest"] },
          identity: { generation: "0", turn_started: "5", response_epoch: "5", tools_epoch: "5" },
          content: part.type === "text" || part.type === "thinking" ? { type: part.type, preview: { text: part.type === "text" ? "Restored canonical child text" : "", truncated: false } }
            : { type: part.type, offset: part.offset, items: [], next_offset: null },
        }
        return { type: "ready", page }
      },
    },
    onCommand(command) { commands.push(command); if (command.type === "resolve_child_control") resolved = true; return { type: "accepted" } },
  })
  setup.renderer.root.add(app)
  try {
    app.setState({ ...app.state, connection: { ...app.state.connection, phase: "connected" } })
    await new Promise<void>(resolve => setImmediate(resolve))
    app.openSubagentPicker(); app.picker.select.selectCurrent()
    await waitForHistory(setup, () => setup.captureCharFrame().includes("Restored canonical child text"))
    expect(app.activeSubagentId).toBe("agent")
    expect(app.interactionPanel.visible).toBe(true)
    expect(app.interactionPanel.usesComposer).toBe(true)
    expect(scopes).toBe(1); expect(stateReads).toBe(1); expect(tailReads).toBe(4)
    app.composer.value = "child.txt"; expect(await app.composer.submit()).toBe(true)
    expect(commands).toContainEqual(expect.objectContaining({ type: "resolve_child_control", session_id: "root", target, expected_revision: "1" }))
    expect(app.state.streamingTail).toBeNull()
    app.setState({ ...app.state, connection: { ...app.state.connection, phase: "reconnecting" } })
    app.setState({ ...app.state, connection: { ...app.state.connection, phase: "connected" } })
    await waitForHistory(setup, () => scopes === 2 && stateReads >= 2 && tailReads >= 8)
    expect(app.activeSubagentId).toBe("agent")
    expect(setup.captureCharFrame()).toContain("Restored canonical child text")
    setup.mockInput.pressEscape()
    await waitForHistory(setup, () => app.activeSubagentId === null)
    const before = stateReads
    await Bun.sleep(300)
    expect(stateReads).toBe(before)
  } finally { app.destroy(); setup.renderer.destroy(); await new Promise<void>(resolve => setImmediate(resolve)) }
  expect(app.historyCache.allocations.usage.bytes).toBe(0)
})
