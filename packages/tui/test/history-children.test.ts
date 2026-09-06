import type { RottweilerApp } from "../src/app"
import { expect, test } from "bun:test"
import { createTestRenderer, MockTreeSitterClient } from "@opentui/core/testing"
import { createRottweilerApp } from "../src/app"
import { PROTOCOL_VERSION, type ClientCommand } from "../src/protocol"
import { emptySessionReader, fixturePage, sessionReaderFor, toolItem, waitForHistory } from "./fixtures/history"

async function waitForParent(app: RottweilerApp): Promise<void> {
  const deadline = performance.now() + 1000
  while (app.activeSubagentId !== null) {
    if (performance.now() > deadline) throw new Error("Escape did not return to parent")
    await Bun.sleep(1)
  }
}

async function childHarness(activity: "running" | "idle", sourceReader?: import("../src/session-reader").SessionReader) {
  const harness = await createTestRenderer({ width: 96, height: 25, useThread: false })
  const commands: ClientCommand[] = []
  const sessions: string[] = []
  const app = createRottweilerApp(harness.renderer, {
    sessionId: "parent", treeSitterClient: new MockTreeSitterClient(),
    sessionReader: {
      children: async target => {
        expect(target).toEqual({ sessionId: "parent", scope: { type: "session" } })
        return { type: "ready", snapshot: { through: "1", children: [{ subagent_id: "child", child_session_id: "child-session", spawned: "1", spawned_turn: "1", task_preview: "Inspect child history", task_truncated: false }] } }
      },
      tail: emptySessionReader.tail,
      uiCatalog: async () => ({ entries: [] }),
  uiPanels: async () => ({ panels: [] }),
  todos: async () => ({ type: "ready", todos: { through: "1000", snapshot: { items: [] } } }),
      page: async (target, read, signal, allocation) => { if (target.sessionId === "child-session") expect(target.scope).toEqual({ type: "descendant", root_session_id: "parent", ancestry: [{ subagent_id: "child", session_id: "child-session", source_sequence: "1" }] }); sessions.push(target.sessionId); return sourceReader?.page(target, read, signal, allocation) ?? { type: "ready", page: fixturePage(target.sessionId, read) } },
      content: async () => { throw new Error("unused content") },
    },
    onCommand(command) { commands.push(command); return { type: "accepted" } },
  })
  harness.renderer.root.add(app)
  app.composer.value = "parent draft"
  app.openSubagentPicker()
  const request = commands.find(command => command.type === "list_subagents")
  if (request === undefined) throw new Error("missing child catalog read")
  app.handleEvent({
    type: "subagents_listed", session_id: "parent",
    meta: { protocol_version: PROTOCOL_VERSION, client_id: "tui-client", request_id: request.meta.request_id, emitted_at: "2026-09-04T00:00:00Z" },
    subagents: [{ subagent_id: "child", child_session_id: "child-session", task: "Inspect child history", agent: "reviewer", model: "fast", isolation: "worktree", activity }],
  })
  app.picker.select.selectCurrent()
  await Bun.sleep(0)
  await harness.renderOnce()
  return { harness, app, commands, sessions }
}

test("child inspection pages its own session and preserves parent draft and mutation ownership", async () => {
  const { harness, app, commands, sessions } = await childHarness("idle")
  try {
    expect(app.activeSubagentId).toBe("child")
    expect(sessions).toContain("child-session")
    expect(app.transcript.mountedCards.size).toBeLessThanOrEqual(16)
    expect(app.composer.value).toBe("")
    app.composer.value = "child follow-up"
    harness.mockInput.pressEnter()
    await Bun.sleep(0)
    expect(commands).toContainEqual(expect.objectContaining({ type: "continue_subagent", session_id: "parent", subagent_id: "child", content: "child follow-up" }))
    expect(commands.some(command => command.type === "attach_session" || command.type === "resume_session")).toBe(false)
    harness.mockInput.pressEscape()
    await Bun.sleep(0)
    await waitForParent(app)
    expect(app.activeSubagentId).toBeNull()
    expect(app.composer.value).toBe("parent draft")
    expect(sessions.at(-1)).toBe("parent")
  } finally { app.destroy(); harness.renderer.destroy() }
})

test("running child stays read-only while its paged history remains navigable", async () => {
  const { harness, app, commands } = await childHarness("running")
  try {
    expect(app.composer.visible).toBe(false)
    app.transcript.scrollTo(0)
    await Bun.sleep(0)
    await harness.renderOnce()
    expect(app.transcript.mountedCards.has("0")).toBe(true)
    harness.mockInput.pressEnter()
    await Bun.sleep(0)
    expect(commands.some(command => command.type === "continue_subagent" || command.type === "send_message")).toBe(false)
    harness.mockInput.pressEscape()
    await waitForParent(app)
    expect(app.activeSubagentId).toBeNull()
    expect(app.composer.value).toBe("parent draft")
  } finally { app.destroy(); harness.renderer.destroy() }
})


test("a child source marker replaces stale live preview with canonical history and accepts later deltas", async () => {
  let finished = false
  const sourceReader = { ...sessionReaderFor([]),
    page: (target: import("../src/session-reader").SessionReadTarget, read: import("../src/protocol").TranscriptRead, signal: AbortSignal, allocation: import("../src/transport/reply-allocation").ReplyAllocation) => {
      const item = toolItem(999, "write", "{}", finished ? "canonical result" : undefined)
      if (finished) {
        item.revision = "2001"
        if (item.content.type === "tool" && item.content.status.type === "finished") item.content.status.output.source.sequence = "2001"
      }
      return sessionReaderFor([item], () => ({ generation: "0", through: finished ? "2001" : "2000" })).page(target, read, signal, allocation)
    },
    todos: async () => ({ type: "ready" as const, todos: { through: finished ? "2001" : "2000", snapshot: { items: [] } } }),
  }
  const { harness, app, sessions } = await childHarness("running", sourceReader)
  const progress = (sequence: string, event: import("../src/protocol").EngineEvent | null) => app.handleEvent({
    type: "subagent_progress", parent_session_id: "parent", subagent_id: "child", child_session_id: "child-session", child_sequence: sequence, event,
  })
  const delta = (sequence: string, text: string): import("../src/protocol").EngineEvent => ({ type: "text_delta",
    meta: { protocol_version: PROTOCOL_VERSION, session_id: "child-session", sequence_id: sequence, emitted_at: "2026-01-01T00:00:00Z" }, turn_id: "child-turn", text,
  })
  try {
    progress("2000", delta("2000", "old live preview"))
    await waitForHistory(harness, () => app.transcript.streamingMarkdown.content === "old live preview")
    const before = sessions.length
    finished = true
    progress("2001", null)
    await waitForHistory(harness, () => sessions.length > before && app.transcript.mountedCards.get("999")?.item.revision === "2001")
    expect(app.transcript.streamingCard.visible).toBeFalse()
    expect(app.transcript.mountedCards.get("999")?.item.content.type).toBe("tool")
    progress("2002", delta("2002", "new response"))
    await waitForHistory(harness, () => app.transcript.streamingMarkdown.content === "new response")
    expect(app.transcript.streamingMarkdown.content).not.toContain("old live preview")
    expect(() => app.handleEvent({ type: "subagent_progress", parent_session_id: "parent", subagent_id: "child", child_session_id: "child-session", event: null })).toThrow("canonical sequence")
    expect(app.activeSubagentId).toBe("child")
  } finally { app.destroy(); harness.renderer.destroy() }
})
