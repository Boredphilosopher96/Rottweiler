import { expect, test } from "bun:test"
import { createTestRenderer, MockTreeSitterClient } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import { MAX_SESSION_READ_ANCESTORS, type TranscriptItem } from "../../src/protocol"
import { descendantSessionRead, directSessionRead, type SessionReadTarget } from "../../src/session-reader"
import { emptySessionReader, sessionReaderFor, waitForHistory } from "../fixtures/history"

function child(id: string, session: string): TranscriptItem {
  return { id, ordinal: "0", revision: id, agent_turn: "1", content: { type: "subagent", subagent_id: `agent-${id}`, session_id: session,
    task: { text: `Inspect ${session}`, format: "text", complete: true, source: { sequence: id, selector: { type: "subagent_task" } } },
    status: { type: "running" },
  } }
}

test("historical nested child navigation carries exact ancestry across history, tasks and full content", async () => {
  const setup = await createTestRenderer({ width: 100, height: 30, useThread: false })
  const grand: TranscriptItem = { id: "9", ordinal: "0", revision: "9", agent_turn: "1", content: {
    type: "command", name: "inspect", message: { text: "preview", format: "text", complete: false,
      source: { sequence: "9", selector: { type: "command_message" } } },
  } }
  const reads: { kind: string; target: SessionReadTarget }[] = []
  const pages = new Map([["parent", [child("2", "child")]], ["child", [child("4", "grand")]], ["grand", [grand]]])
  const app = createRottweilerApp(setup.renderer, { sessionId: "parent", replaySessionId: "parent", treeSitterClient: new MockTreeSitterClient(), sessionReader: {
    ...emptySessionReader,
    page: async (target, read, signal, allocation) => {
      reads.push({ kind: "page", target })
      return sessionReaderFor(pages.get(target.sessionId) ?? []).page(target, read, signal, allocation)
    },
    todos: async target => {
      reads.push({ kind: "todos", target })
      return { type: "ready", todos: { through: "9", snapshot: { items: [] } } }
    },
    content: async (target, read) => {
      reads.push({ kind: "content", target })
      return { view: read.view, source: read.source, offset: 0, next_offset: null, total_bytes: 13, format: "text", text: "complete body" }
    },
  } })
  setup.renderer.root.add(app)
  const open = async (id: string) => {
    await waitForHistory(setup, () => app.transcript.mountedCards.has(id))
    const row = app.transcript.mountedCards.get(id)!
    await setup.renderOnce()
    await setup.mockMouse.click(row.footer.x + 2, row.footer.y)
    await setup.flush()
  }
  try {
    await open("2")
    await waitForHistory(setup, () => app.transcript.mountedCards.has("4"))
    expect(app.recycleState()).toBeNull()
    await open("4")
    await waitForHistory(setup, () => app.transcript.mountedCards.has("9"))
    const target: SessionReadTarget = { sessionId: "grand", scope: { type: "descendant", root_session_id: "parent", ancestry: [
      { session_id: "child", subagent_id: "agent-2", source_sequence: "2" },
      { session_id: "grand", subagent_id: "agent-4", source_sequence: "4" },
    ] } }
    expect(reads).toContainEqual({ kind: "page", target })
    await waitForHistory(setup, () => reads.some(read => read.kind === "todos" && read.target.sessionId === "grand"))
    expect(reads).toContainEqual({ kind: "todos", target })
    await open("9")
    await waitForHistory(setup, () => app.outputViewer.body.plainText === "complete body")
    expect(reads).toContainEqual({ kind: "content", target })
    setup.mockInput.pressEscape()
    await waitForHistory(setup, () => !app.outputViewer.visible)
    setup.mockInput.pressEscape()
    await waitForHistory(setup, () => app.activeSubagentId === null)
    expect(reads.at(-1)).toMatchObject({ kind: "page", target: { sessionId: "parent", scope: { type: "session" } } })
  } finally { app.destroy(); setup.renderer.destroy() }
})

test("local ancestry construction rejects cycles and the generated depth ceiling", () => {
  let target = directSessionRead("root")
  for (let i = 0; i < MAX_SESSION_READ_ANCESTORS; i++) target = descendantSessionRead(target, { session_id: `child-${i}`, subagent_id: `agent-${i}`, source_sequence: String(i) })
  expect(() => descendantSessionRead(target, { session_id: "next", subagent_id: "next", source_sequence: "9" })).toThrow("ancestry")
  expect(() => descendantSessionRead(directSessionRead("root"), { session_id: "root", subagent_id: "cycle", source_sequence: "0" })).toThrow("ancestry")
})
