import { expect, test } from "bun:test"
import { createTestRenderer, MockTreeSitterClient } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import type { ClientCommand, CommandOutcome } from "../../src/protocol"
import { systemThemeFor } from "../../src/theme"
import { emptySessionReader, sessionReaderFor, toolItem, waitForHistory } from "../fixtures/history"
import { fixturePresentation, surfacePage } from "../fixtures/ui"

test("native panel actions are keyboard reachable, exact-revision commands, and survive closing until settlement", async () => {
  const setup = await createTestRenderer({ width: 100, height: 32, useThread: false })
  const presentation = fixturePresentation()
  presentation.descriptor.surface = { surface: "panel" }
  presentation.descriptor.actions = [{ id: "inspect", label: "Inspect source" }]
  const commands: ClientCommand[] = []
  let finish!: (outcome: CommandOutcome) => void
  let reads = 0
  const app = createRottweilerApp(setup.renderer, {
    theme: systemThemeFor("dark"), treeSitterClient: new MockTreeSitterClient(), sessionReader: { ...emptySessionReader,
      uiCatalog: async () => ({ entries: [{ owner: presentation.owner, descriptor: presentation.descriptor }] }),
      uiPanels: async () => { reads++; return { panels: [{ revision: 7, presentation }] } },
    },
    onCommand: command => {
      commands.push(command)
      return command.type === "invoke_ui_action" ? new Promise(resolve => { finish = resolve }) : { type: "accepted" }
    },
  })
  setup.renderer.root.add(app)
  try {
    app.openCommandPicker()
    app.commandPalette.selectById("ui.panels")
    app.commandPalette.activateSelected()
    await waitForHistory(setup, () => app.picker.select.options.some(option => option.name === "Inspection result"))
    app.picker.select.selectCurrent()
    await waitForHistory(setup, () => app.outputViewer.visible && app.outputViewer.actions.visible)
    expect(setup.captureCharFrame()).toContain("engine.rs")
    app.setSystemTheme(systemThemeFor("light"))
    await setup.renderOnce()
    expect(app.outputViewer.visible).toBeTrue()
    expect(reads).toBe(1)
    setup.mockInput.pressTab()
    expect(app.outputViewer.actions.focused).toBeTrue()
    app.setSystemTheme(systemThemeFor("dark"))
    await setup.renderOnce()
    expect(app.outputViewer.actions.focused).toBeTrue()
    setup.mockInput.pressEnter()
    await setup.flush()
    const actions = commands.filter(command => command.type === "invoke_ui_action")
    expect(actions).toHaveLength(1)
    expect(actions[0]).toMatchObject({ session_id: "session-local", request: {
      owner: presentation.owner, contribution_id: "result", action_id: "inspect", target: { surface: "panel", revision: 7 },
    } })
    setup.mockInput.pressEnter()
    expect(commands.filter(command => command.type === "invoke_ui_action")).toHaveLength(1)
    setup.mockInput.pressEscape()
    await waitForHistory(setup, () => !app.outputViewer.visible)
    expect(app.outputViewer.actions.options).toHaveLength(0)
    expect(app.recycleState()).toBeNull()
    finish({ type: "accepted" })
    await setup.flush()
    expect(app.recycleState()).not.toBeNull()
    expect(reads).toBe(1)
  } finally { app.destroy(); setup.renderer.destroy() }
})

test.each([true, false])("canonical tool action uses the source invocation only with an available generation: %s", async available => {
  const setup = await createTestRenderer({ width: 100, height: 32, useThread: false })
  const presentation = fixturePresentation()
  presentation.descriptor.actions = [{ id: "inspect", label: "Inspect source" }]
  const item = toolItem(2, "read", "{}", "Plain result")
  if (item.content.type !== "tool" || item.content.status.type !== "finished") throw new Error("tool fixture")
  item.content.invocation_id = "host-invocation"
  item.content.status.presentation = { title: presentation.descriptor.title, source: {
    sequence: "2", selector: { type: "tool_presentation", invocation_id: "host-invocation" },
  } }
  const commands: ClientCommand[] = []
  const app = createRottweilerApp(setup.renderer, { treeSitterClient: new MockTreeSitterClient(), sessionReader: {
    ...sessionReaderFor([item]), content: async (_session, read) => surfacePage(presentation, read),
    uiCatalog: async () => ({ entries: [{ owner: { ...presentation.owner, generation: available ? presentation.owner.generation : "b".repeat(32) }, descriptor: presentation.descriptor }] }),
  }, onCommand: command => { commands.push(command); return { type: "accepted" } } })
  setup.renderer.root.add(app)
  try {
    await waitForHistory(setup, () => app.transcript.mountedCards.has("2"))
    const row = app.transcript.mountedCards.get("2")!
    row.toggle()
    await setup.renderOnce()
    await setup.mockMouse.click(row.presentationFooter.x + 2, row.presentationFooter.y)
    await waitForHistory(setup, () => app.outputViewer.actions.visible)
    setup.mockInput.pressTab()
    setup.mockInput.pressEnter()
    await setup.flush()
    const actions = commands.filter(command => command.type === "invoke_ui_action")
    expect(actions).toHaveLength(available ? 1 : 0)
    if (available) expect(actions[0]).toMatchObject({ request: {
      owner: presentation.owner, contribution_id: "result", action_id: "inspect",
      target: { surface: "tool", invocation_id: "host-invocation" },
    } })
    else expect(app.outputViewer.hint.plainText).toContain("unavailable for this extension generation")
  } finally { app.destroy(); setup.renderer.destroy() }
})


test("disconnect retires displayed panel strings and catalog picker references before another view opens", async () => {
  const setup = await createTestRenderer({ width: 70, height: 20, useThread: false })
  const presentation = fixturePresentation()
  presentation.descriptor.surface = { surface: "panel" }
  const app = createRottweilerApp(setup.renderer, { treeSitterClient: new MockTreeSitterClient(), sessionReader: { ...emptySessionReader,
    uiCatalog: async () => ({ entries: [{ owner: presentation.owner, descriptor: presentation.descriptor }] }),
    uiPanels: async () => ({ panels: [{ revision: 1, presentation }] }),
  } })
  setup.renderer.root.add(app)
  const open = async () => {
    app.openCommandPicker(); app.commandPalette.selectById("ui.panels"); app.commandPalette.activateSelected()
    await waitForHistory(setup, () => app.picker.select.options.some(option => option.name === "Inspection result"))
  }
  try {
    await open()
    app.resetConnectionProjections()
    expect(app.picker.visible).toBeFalse()
    expect(app.picker.select.options).toHaveLength(0)
    await open()
    app.picker.select.selectCurrent()
    await waitForHistory(setup, () => app.outputViewer.surface.visible)
    const nodes = app.outputViewer.surface.getChildren()
    app.resetConnectionProjections()
    expect(app.outputViewer.visible).toBeFalse()
    expect(app.outputViewer.surface.getChildren()).toHaveLength(0)
    expect(nodes.every(node => node.isDestroyed)).toBeTrue()
  } finally { app.destroy(); setup.renderer.destroy() }
})


test("a four-row terminal keeps all declared actions keyboard reachable in one native row", async () => {
  const { OutputViewerRenderable } = await import("../../src/components/output-viewer")
  const { prepareUiSurface } = await import("../../src/ui/presentation")
  const setup = await createTestRenderer({ width: 60, height: 4, useThread: false })
  const surface = fixturePresentation()
  surface.descriptor.actions = Array.from({ length: 4 }, (_, index) => ({ id: `action-${index}`, label: `Action ${index}` }))
  const viewer = new OutputViewerRenderable(setup.renderer, systemThemeFor("dark"))
  setup.renderer.root.add(viewer)
  const selected: string[] = []
  try {
    viewer.showDocument({ open: true, surface: prepareUiSurface(surface), page: null, previous: false, loading: false, error: null })
    viewer.setActions(surface.descriptor.actions, true, id => { selected.push(id) })
    await setup.renderOnce()
    expect(viewer.height).toBeLessThanOrEqual(4)
    expect(viewer.actions.height).toBe(1)
    viewer.actions.focus()
    for (let index = 0; index < 3; index++) setup.mockInput.pressArrow("down")
    setup.mockInput.pressEnter()
    await setup.flush()
    expect(selected).toEqual(["action-3"])
    expect(viewer.actions.y + viewer.actions.height).toBeLessThanOrEqual(4)
  } finally { viewer.destroy(); setup.renderer.destroy() }
})
