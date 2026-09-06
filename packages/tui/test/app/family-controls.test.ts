import { expect, test } from "bun:test"
import { createTestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import type { ClientCommand } from "../../src/protocol"
import type { ChildControlTarget, SessionControlsSnapshot } from "../../../../protocol/types"
import type { FamilyControlsReader } from "../../src/family-controls-reader"
import { emptySessionReader } from "../fixtures/history"
import { createInitialState } from "../../src/state"

const target: ChildControlTarget = { session_id: "child-session", ancestry: [{ subagent_id: "agent", session_id: "child-session" }] }
const flush = () => new Promise<void>(resolve => setImmediate(resolve))
function untilAbort(signal: AbortSignal): Promise<never> {
  return new Promise((_, reject) => { if (signal.aborted) reject(signal.reason); else signal.addEventListener("abort", () => reject(signal.reason), { once: true }) })
}
function snapshot(kind: "question" | "approval"): SessionControlsSnapshot {
  return { through: "12", controls: {
    pending_plan: null,
    questions: kind !== "question" ? [] : [{ turn_id: "turn", question_id: "child-question", questions: [{ id: "child-question", prompt: "Which child file?", response_kind: "text", options: [] }] }],
    approvals: kind !== "approval" ? [] : [{ invocation_id: "child-invocation", tool_call_id: "provider-alias", turn_id: "turn", name: "write", args: { path: "child.txt" }, capabilities: [], rationale: "Child asks to write", diff: null }],
  } }
}

for (const kind of ["question", "approval"] as const) {
  test(`unopened child ${kind} is discovered, displayed and answered under exact live authority`, async () => {
    const setup = await createTestRenderer({ width: 100, height: 30, useThread: false })
    const emitted: ClientCommand[] = [], cursors: (string | null)[] = []
    let revision = "7", resolved = false, snapshotReads = 0
    const reader: FamilyControlsReader = {
      async state() { throw new Error("display scope has not resolved") },
      scope: (_root, _target, signal) => untilAbort(signal),
      async watch(_root, after, signal, allocation) {
        cursors.push(after); allocation.admit(4096)
        if (after !== null) return untilAbort(signal)
        return { revision, children: [{ target, controls: { revision, through: "12", questions: kind === "question" ? 1 : 0, approvals: kind === "approval" ? 1 : 0, pending_plan: false, available: true } }] }
      },
      async child(_root, selected, _signal, allocation) {
        expect(selected).toEqual(target); allocation.admit(8192); snapshotReads++
        return { revision, snapshot: resolved ? { through: "13", controls: { questions: [], approvals: [], pending_plan: null } } : snapshot(kind) }
      },
    }
    const app = createRottweilerApp(setup.renderer, { sessionId: "root", familyControls: reader,
      // Display history may be catching up while a live control needs an immediate answer.
      sessionReader: { ...emptySessionReader, children: async (_target, signal) => untilAbort(signal) },
      onCommand(command) { emitted.push(command); if (command.type === "resolve_child_control") { resolved = true; revision = "8" } return { type: "accepted" } },
    })
    setup.renderer.root.add(app)
    try {
      const state = createInitialState()
      app.setState({ ...state, connection: { ...state.connection, phase: "connected" } })
      app.composer.value = "parent draft"
      await flush(); await setup.renderOnce()
      expect(app.banner.plainText).toContain("child agent needs a response")
      expect(Object.keys(app.state.questions)).toHaveLength(0)
      app.openSubagentPicker()
      expect(app.picker.select.options[0]?.name).toContain("Response needed")
      app.picker.select.selectCurrent()
      await flush(); await setup.renderOnce()
      expect(app.activeSubagentId).toBe("agent")
      expect(app.interactionPanel.visible).toBe(true)
      expect(snapshotReads).toBe(1)
      expect(app.transcript.mountedEntryCount).toBe(0)
      if (kind === "question") {
        expect(app.interactionPanel.usesComposer).toBe(true)
        expect(app.composer.visible).toBe(true)
        app.composer.value = "!literal child answer"
        expect(await app.composer.submit()).toBe(true)
      } else {
        expect(app.interactionPanel.select.options.map(option => option.value)).not.toContain("allow_tool_session")
        expect(app.interactionPanel.select.options.map(option => option.value)).not.toContain("auto_safe_mode")
        app.interactionPanel.select.selectCurrent()
      }
      await flush()
      const action = emitted.find(command => command.type === "resolve_child_control")
      expect(action).toMatchObject({ session_id: "root", target, expected_revision: "7", response: { type: kind } })
      expect(emitted.some(command => ["answer_question", "approve_tool", "user_shell_started", "add_session_permission_rule"].includes(command.type))).toBe(false)
      expect(app.interactionPanel.visible).toBe(false)
      expect(Object.keys(app.state.questions)).toHaveLength(0)
      // A new host may begin at a lower live revision after reconnect.
      app.setState({ ...app.state, connection: { ...app.state.connection, phase: "reconnecting" } })
      revision = "1"
      app.setState({ ...app.state, connection: { ...app.state.connection, phase: "connected" } })
      await flush()
      expect(cursors.filter(cursor => cursor === null)).toHaveLength(2)
      setup.mockInput.pressEscape(); await Bun.sleep(30)
      expect(app.activeSubagentId).toBeNull()
      expect(app.composer.value).toBe("parent draft")
    } finally { app.destroy(); setup.renderer.destroy(); await flush() }
    expect(app.historyCache.allocations.usage.bytes).toBe(0)
  })
}

test("leaving a child preserves its unsettled response owner and defers renderer handoff", async () => {
  const { recycleTuiIfNeeded } = await import("../../src/recycle-state")
  const setup = await createTestRenderer({ width: 100, height: 30, useThread: false })
  const dispatched = Promise.withResolvers<void>(), settle = Promise.withResolvers<void>()
  const reader: FamilyControlsReader = {
      async state() { throw new Error("display scope has not resolved") },
      scope: (_root, _target, signal) => untilAbort(signal),
    async watch(_root, after, signal) {
      if (after !== null) return untilAbort(signal)
      return { revision: "7", children: [{ target, controls: { revision: "7", through: "12", questions: 0, approvals: 1, pending_plan: false, available: true } }] }
    },
    async child() { return { revision: "7", snapshot: snapshot("approval") } },
  }
  const app = createRottweilerApp(setup.renderer, { sessionId: "root", familyControls: reader,
    sessionReader: { ...emptySessionReader, children: async (_target, signal) => untilAbort(signal) },
    async onCommand(command, allocation) {
      if (command.type !== "resolve_child_control") return { type: "accepted" }
      allocation.admit(8192); dispatched.resolve(); await settle.promise
      return { type: "rejected", error: { category: "protocol", code: "child_changed", message: "Child authority changed", retryable: true } }
    },
  })
  setup.renderer.root.add(app)
  try {
    app.setState({ ...app.state, connection: { ...app.state.connection, phase: "connected" } })
    await flush(); app.openSubagentPicker(); app.picker.select.selectCurrent()
    await flush(); app.interactionPanel.select.selectCurrent(); await dispatched.promise
    setup.mockInput.pressEscape(); await Bun.sleep(30)
    expect(app.activeSubagentId).toBeNull()
    expect(app.interactionPanel.visible).toBe(false)
    expect(app.recycleState()).toBeNull()
    let recycled = false
    expect(recycleTuiIfNeeded({ allocations: app.historyCache.allocations, observedBytes: 500, thresholdBytes: 384, path: "/unused-family-handoff.json",
      capture: () => app.recycleState(), recycle: () => { recycled = true },
    })).toBe(false)
    expect(recycled).toBe(false)
    const allocations = app.historyCache.allocations
    expect((allocations.usage.domains.decoding ?? 0) + (allocations.usage.domains.urgent ?? 0)).toBe(8192)
    settle.resolve(); await flush()
    expect(app.state.errors.some(error => error.code === "child_changed")).toBe(false)
    expect((allocations.usage.domains.decoding ?? 0) + (allocations.usage.domains.urgent ?? 0)).toBe(0)
    expect(app.recycleState()).not.toBeNull()
  } finally { settle.resolve(); app.destroy(); setup.renderer.destroy(); await flush() }
  expect(app.historyCache.allocations.usage.bytes).toBe(0)
})
