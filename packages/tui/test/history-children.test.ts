import type { RottweilerApp } from "../src/app"
import { expect, test } from "bun:test"
import { createTestRenderer, MockTreeSitterClient } from "@opentui/core/testing"
import { createRottweilerApp } from "../src/app"
import { PROTOCOL_VERSION, type ClientCommand } from "../src/protocol"
import { fixturePage } from "./fixtures/history"

async function waitForParent(app: RottweilerApp): Promise<void> {
  const deadline = performance.now() + 1000
  while (app.activeSubagentId !== null) {
    if (performance.now() > deadline) throw new Error("Escape did not return to parent")
    await Bun.sleep(1)
  }
}

async function childHarness(activity: "running" | "idle") {
  const harness = await createTestRenderer({ width: 96, height: 25, useThread: false })
  const commands: ClientCommand[] = []
  const sessions: string[] = []
  const app = createRottweilerApp(harness.renderer, {
    sessionId: "parent", treeSitterClient: new MockTreeSitterClient(),
    sessionReader: {
      todos: async () => ({ type: "ready", todos: { through: "1000", snapshot: { items: [] } } }),
      page: async (session, read) => { sessions.push(session); return { type: "ready", page: fixturePage(session, read) } },
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
