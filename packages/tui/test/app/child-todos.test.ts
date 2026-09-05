import { expect, test } from "bun:test"
import { createTestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import { type ClientCommand, type TodoReadResult } from "../../src/protocol"
import { emptySessionReader, waitForHistory } from "../fixtures/history"

test("opening an idle child loads exact tasks and leaving retires a pending child read", async () => {
  const setup = await createTestRenderer({ width: 120, height: 30, useThread: false })
  const commands: ClientCommand[] = []
  const reads: { session: string; signal: AbortSignal; resolve(result: TodoReadResult): void }[] = []
  const app = createRottweilerApp(setup.renderer, {
    sessionId: "parent", sessionReader: { ...emptySessionReader,
      todos: ({ sessionId: session }, signal) => new Promise(resolve => reads.push({ session, signal, resolve })),
    },
    onCommand: command => { commands.push(command); return { type: "accepted" } },
  })
  setup.renderer.root.add(app)
  function selectChild(): void {
    app.openSubagentPicker()
    const command = commands.at(-1)!
    app.handleEvent({ type: "subagents_listed", meta: { ...command.meta, emitted_at: "2026-01-01T00:00:00Z" }, session_id: "parent",
      subagents: [{ subagent_id: "worker", child_session_id: "child", task: "Inspect internals", agent: "reviewer", model: "fast", isolation: "shared", activity: "idle" }],
    })
    app.picker.select.selectCurrent()
  }
  try {
    selectChild()
    expect(reads[0]?.session).toBe("child")
    reads[0]!.resolve({ type: "ready", todos: { through: "7", snapshot: { items: [{ id: "inspect", content: "Child inspection task", status: "in_progress" }] } } })
    await waitForHistory(setup, () => setup.captureCharFrame().includes("Child inspection task"))
    expect(app.state.todos.snapshot.items).toHaveLength(0)
    setup.mockInput.pressEscape()
    await waitForHistory(setup, () => app.activeSubagentId === null)
    selectChild()
    expect(reads[1]?.session).toBe("child")
    setup.mockInput.pressEscape()
    await waitForHistory(setup, () => app.activeSubagentId === null)
    expect(reads[1]?.signal.aborted).toBeTrue()
    reads[1]!.resolve({ type: "ready", todos: { through: "8", snapshot: { items: [{ id: "late", content: "Late child task must stay hidden", status: "pending" }] } } })
    await setup.flush()
    await setup.renderOnce()
    expect(setup.captureCharFrame()).not.toContain("Late child task")
    expect(app.state.todos.snapshot.items).toHaveLength(0)
    expect(commands.some(command => command.type === "get_todos")).toBeFalse()
  } finally { app.destroy(); setup.renderer.destroy() }
})
