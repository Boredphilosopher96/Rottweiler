import { expect, test } from "bun:test"
import { TextRenderable } from "@opentui/core"
import { createTestRenderer, MockTreeSitterClient } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import { systemThemeFor } from "../../src/theme"
import { historyReaderFor, toolItem, waitForHistory } from "../fixtures/history"
import { fixturePresentation, surfacePage } from "../fixtures/ui"

test("native tool surface opens from canonical row and survives retheming without a second read", async () => {
  const setup = await createTestRenderer({ width: 100, height: 32, useThread: false })
  const presentation = fixturePresentation()
  const item = toolItem(2, "read", '{"path":"engine.rs"}', "Plain result")
  if (item.content.type !== "tool" || item.content.status.type !== "finished") throw new Error("tool fixture")
  item.content.status.presentation = { title: presentation.descriptor.title, source: {
    sequence: "2", selector: { type: "tool_presentation", invocation_id: item.content.invocation_id },
  } }
  let reads = 0
  const app = createRottweilerApp(setup.renderer, {
    theme: systemThemeFor("dark"), treeSitterClient: new MockTreeSitterClient(),
    historyReader: {
      ...historyReaderFor([item]),
      content: async (_session, read) => { reads++; return surfacePage(presentation, read) },
    },
  })
  setup.renderer.root.add(app)
  try {
    await waitForHistory(setup, () => app.transcript.mountedCards.has("2"))
    const row = app.transcript.mountedCards.get("2")
    if (row === undefined) throw new Error("row missing")
    row.toggle()
    await setup.renderOnce()
    expect(row.presentationFooter.visible).toBeTrue()
    await setup.mockMouse.click(row.presentationFooter.x + 2, row.presentationFooter.y)
    await setup.flush()
    await setup.renderOnce()
    const oldViewer = app.outputViewer
    expect(oldViewer.visible).toBeTrue()
    expect(oldViewer.header.plainText).toBe("Inspection result")
    expect(oldViewer.surface.getChildren().length).toBe(4)
    expect(setup.captureCharFrame()).toContain("engine.rs")
    const oldNodes = oldViewer.surface.getChildren().filter(node => node instanceof TextRenderable)
    app.setSystemTheme(systemThemeFor("light"))
    await setup.renderOnce()
    expect(app.outputViewer).not.toBe(oldViewer)
    expect(app.outputViewer.surface.getChildren().length).toBe(4)
    expect(reads).toBe(1)
    expect(oldNodes.every(node => node.content.chunks.every(chunk => chunk.text === ""))).toBeTrue()
    setup.mockInput.pressEscape()
    await waitForHistory(setup, () => !app.outputViewer.visible)
    expect(app.outputViewer.visible).toBeFalse()
    expect(app.outputViewer.surface.getChildren().length).toBe(0)
  } finally { app.destroy(); setup.renderer.destroy() }
})
