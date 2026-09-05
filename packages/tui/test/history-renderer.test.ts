import { fixturePage } from "./fixtures/history"
import { expect, test } from "bun:test"
import { createTestRenderer, MockTreeSitterClient } from "@opentui/core/testing"
import { createRottweilerApp } from "../src/app"
import type { TranscriptRead } from "../src/protocol"
import type { SessionReader } from "../src/session-reader"


test("production app reads native semantic rows and navigates beyond its mounted window", async () => {
  const harness = await createTestRenderer({ width: 100, height: 30 })
  const reads: TranscriptRead[] = []
  const reader: SessionReader = {
      todos: async () => ({ type: "ready", todos: { through: "1000", snapshot: { items: [] } } }),
    page: async (session, read) => { reads.push(read); return { type: "ready", page: fixturePage(session, read) } },
    content: async () => { throw new Error("unused") },
  }
  const app = createRottweilerApp(harness.renderer, { sessionId: "history", sessionReader: reader, treeSitterClient: new MockTreeSitterClient() })
  harness.renderer.root.add(app)
  try {
    await Bun.sleep(0)
    await harness.renderOnce()
    await harness.renderOnce()
    expect(app.transcript.mountedCards.size).toBe(16)
    expect([...app.transcript.mountedCards.values()].some(card => card.item.id === "999")).toBe(true)
    expect(harness.captureCharFrame()).toContain("Visible history body 999")
    app.transcript.scrollTo(0)
    await Bun.sleep(0)
    await harness.renderOnce()
    await harness.renderOnce()
    expect(reads.at(-1)?.position.type).toBe("first")
    expect(app.transcript.mountedCards.size).toBe(16)
    const deadline = performance.now() + 1000
    while (!harness.captureCharFrame().includes("Visible history body 0")) {
      if (performance.now() > deadline) throw new Error("new history row was not painted")
      await Bun.sleep(1)
      await harness.renderOnce()
    }
    expect(harness.captureCharFrame()).toContain("Visible history body 0")
    for (let index = 0; index < 150; index++) {
      app.transcript.scrollBy(1, "step")
      await Bun.sleep(0)
      await harness.renderOnce()
      await harness.renderOnce()
      expect(app.transcript.mountedCards.size).toBeLessThanOrEqual(16)
    }
    expect(reads.some(read => read.position.type === "around")).toBe(true)
    expect([...app.transcript.mountedCards.values()].some(card => Number(card.item.id) > 32)).toBe(true)
  } finally {
    app.destroy()
    harness.renderer.destroy()
  }
})

test("complete-content interaction pages bounded bodies and releases the overlay on Escape", async () => {
  const harness = await createTestRenderer({ width: 100, height: 30, useThread: false })
  const offsets: number[] = []
  const app = createRottweilerApp(harness.renderer, {
    sessionId: "history", treeSitterClient: new MockTreeSitterClient(),
    sessionReader: {
      todos: async () => ({ type: "ready", todos: { through: "1000", snapshot: { items: [] } } }),
      page: async (session, read) => {
        const page = fixturePage(session, read)
        for (const item of page.items) if (item.content.type === "command") item.content.message.complete = false
        return { type: "ready", page }
      },
      content: async (_session, read) => {
        offsets.push(read.offset)
        return {
          view: read.view, source: read.source, offset: read.offset, next_offset: read.offset === 0 ? 4096 : null,
          total_bytes: 8192, format: "text", text: (read.offset === 0 ? "a" : "b").repeat(4096)
        }
      },
    },
  })
  harness.renderer.root.add(app)
  try {
    await Bun.sleep(0)
    await harness.renderOnce()
    const row = app.transcript.mountedCards.get("999")
    if (row === undefined) throw new Error("missing latest row")
    await harness.mockMouse.click(row.footer.x + 1, row.footer.y)
    await Bun.sleep(0)
    await harness.renderOnce()
    expect(app.outputViewer.visible).toBe(true)
    expect(app.composer.visible).toBe(false)
    expect(app.outputViewer.body.plainText).toBe("a".repeat(4096))
    harness.mockInput.pressArrow("right")
    await Bun.sleep(0)
    expect(offsets).toEqual([0, 4096])
    expect(app.outputViewer.body.plainText).toBe("b".repeat(4096))
    harness.mockInput.pressEscape()
    const deadline = performance.now() + 1000
    while (app.outputViewer.visible) {
      if (performance.now() > deadline) throw new Error("content overlay did not close")
      await Bun.sleep(1)
    }
    expect(app.outputViewer.body.plainText).toBe("")
    expect(app.composer.visible).toBe(true)
  } finally { app.destroy(); harness.renderer.destroy() }
})
