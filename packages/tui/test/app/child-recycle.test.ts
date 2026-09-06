import { expect, test } from "bun:test"
import { createTestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import type { FamilyControlsReader } from "../../src/family-controls-reader"
import type { ClientCommand, SessionStateSnapshot } from "../../src/protocol"
import { emptySessionReader, waitForHistory } from "../fixtures/history"
import type { AppClientState } from "../../src/recycle-state"

const target = { session_id: "child", ancestry: [{ subagent_id: "agent", session_id: "child" }] }
const metadata: SessionStateSnapshot = { through: "10", driver_client_id: "driver", title: "Child", model_alias: "fast", provider: null,
  thinking: "off", mode_id: "execute", active_turn: null, completed_turns: "1", shell: null, compaction: null, plugin_statuses: [], queued_messages: [], budget: null }
function cancelled(signal: AbortSignal): Promise<never> {
  return new Promise((_, reject) => { if (signal.aborted) reject(signal.reason); else signal.addEventListener("abort", () => reject(signal.reason), { once: true }) })
}

for (const kind of ["question", "approval"] as const) {
  test(`selected child ${kind} survives renderer replacement with fresh family and source authority`, async () => {
    let saved: AppClientState | null = null
    let scopeReads = 0
    const commands: ClientCommand[] = []
    for (let generation = 0; generation < 3; generation++) {
      const setup = await createTestRenderer({ width: 100, height: 30, useThread: false })
      const revision = generation === 0 ? "75" : "1"
      const reader: FamilyControlsReader = {
        async watch(_root, after, signal) {
          if (after !== null) return cancelled(signal)
          return { revision, children: [{ target: generation === 2 ? { session_id: "substituted-child", ancestry: [{ subagent_id: "agent", session_id: "substituted-child" }] } : target, controls: { revision, through: "10", questions: kind === "question" ? 1 : 0,
            approvals: kind === "approval" ? 1 : 0, pending_plan: false, available: true } }] }
        },
        async child() { return { revision, snapshot: { through: "10", controls: { pending_plan: null,
          questions: kind === "question" ? [{ question_id: "q", turn_id: "turn", question: { id: "q", prompt: "Which file?", response_kind: "text", options: [] } }] : [],
          approvals: kind === "approval" ? [{ invocation_id: "invocation", tool_call_id: "alias", turn_id: "turn", name: "write",
            args: { path: "file.txt" }, capabilities: [], rationale: "Write this file", diff: null }] : [],
        } } } },
        async state() { return metadata },
        async scope(root, selected) { expect(selected).toEqual(target); scopeReads++; return { type: "ready", scope: {
          type: "descendant", root_session_id: root, ancestry: [{ session_id: "child", subagent_id: "agent", source_sequence: "2" }],
        } } },
      }
      const app = createRottweilerApp(setup.renderer, { sessionId: "root", familyControls: reader, sessionReader: emptySessionReader,
        onCommand(command) { commands.push(command); return { type: "accepted" } },
      })
      setup.renderer.root.add(app)
      try {
        if (saved !== null) app.restoreRecycleState(saved)
        else app.composer.value = "parent draft"
        app.setState({ ...app.state, connection: { ...app.state.connection, phase: "connected" } })
        if (generation === 0) {
          await Bun.sleep(0); app.openSubagentPicker(); app.picker.select.selectCurrent()
        }
        if (generation === 2) {
          for (let tick = 0; tick < 4; tick++) { await setup.renderOnce(); await Bun.sleep(0); app.applyPendingRecycleScroll() }
          expect(app.activeSubagentId).toBeNull()
          expect(app.interactionPanel.visible).toBe(false)
          expect(app.composer.value).toBe("parent draft")
          expect(scopeReads).toBe(2)
          continue
        }
        await waitForHistory(setup, () => { app.applyPendingRecycleScroll(); return app.activeSubagentId === "agent" && app.interactionPanel.visible })
        await waitForHistory(setup, () => { app.applyPendingRecycleScroll(); return app.recycleState() !== null })
        if (generation === 0) {
          app.composer.value = "unfinished child answer"
          if (kind === "approval") app.interactionPanel.select.setSelectedIndex(2)
          saved = app.recycleState()
          expect(saved?.child).toEqual({ type: "live", target })
          expect(saved?.parentComposer?.content).toBe("parent draft")
        } else {
          await waitForHistory(setup, () => { app.applyPendingRecycleScroll(); return app.composer.value === "unfinished child answer" })
          if (kind === "approval") {
            expect(app.interactionPanel.select.getSelectedOption()?.value).toBe("allow_project")
            app.interactionPanel.select.selectCurrent()
          } else expect(await app.composer.submit()).toBe(true)
          await Bun.sleep(0)
          expect(commands).toContainEqual(expect.objectContaining({ type: "resolve_child_control", target, expected_revision: "1" }))
          setup.mockInput.pressEscape()
          await waitForHistory(setup, () => app.activeSubagentId === null)
          expect(app.composer.value).toBe("parent draft")
        }
      } finally { app.destroy(); setup.renderer.destroy(); await Bun.sleep(0); expect(app.historyCache.allocations.usage.bytes).toBe(0) }
      expect(app.historyCache.allocations.usage.bytes).toBe(0)
    }
    expect(scopeReads).toBe(2)
  })
}
