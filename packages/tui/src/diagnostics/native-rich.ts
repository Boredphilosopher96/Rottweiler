import type { ClientCommand } from "../protocol"
import { connectedApp, requireThat } from "./connected-app"
import { nativeRichInput } from "./connected-input"
import { finishNativeRichProbe } from "./native-rich-report"

/** A real SDK process and journal supply these surfaces through the normal client runtime. */
export async function runNativeRichProbe(directory: string): Promise<void> {
  const actions: Extract<ClientCommand, { type: "invoke_ui_action" }>[] = []
  let input: Awaited<ReturnType<typeof nativeRichInput>>
  try { input = await nativeRichInput(directory) }
  catch (error) {
    return finishNativeRichProbe(directory, { actions }, error, {
      releaseRelay: async () => {}, closeClient: async () => {}, finalEvidence: () => ({}),
    })
  }
  const impairmentSocket = input.impairmentSocketPath
  let client: Awaited<ReturnType<typeof connectedApp>>
  try {
    client = await connectedApp(input, command => {
      if (command.type === "invoke_ui_action") {
        requireThat(actions.length < 2, "unexpected additional native rich action")
        actions.push(command)
      }
    }, true)
  } catch (error) {
    return finishNativeRichProbe(directory, { actions }, error, {
      releaseRelay: async () => {}, closeClient: async () => {}, finalEvidence: () => ({}),
    })
  }
  const { app, setup, until } = client
  const observations: Record<string, unknown> = {}
  const control = async (path: string, body?: object) => {
    const response = await fetch(`http://localhost/${path}`, { unix: impairmentSocket,
      method: path === "status" ? "GET" : "POST", signal: AbortSignal.timeout(3000),
      headers: { "content-type": "application/json" }, body: body === undefined ? undefined : JSON.stringify(body) })
    requireThat(response.ok, `native relay ${path} failed`)
    const text = await response.text()
    requireThat(Buffer.byteLength(text) <= 16 * 1024, "native relay status exceeds its fixture bound")
    return text ? JSON.parse(text) as Record<string, unknown> : {}
  }
  const panels = async (phase: string) => {
    app.openCommandPicker(); app.commandPalette.selectById("ui.panels"); app.commandPalette.activateSelected()
    await until("actual native panel catalog", () => app.picker.select.options.some(option => option.value === "artifact" || option.name.toLowerCase().includes("artifact")))
    const index = app.picker.select.options.findIndex(option => option.value === "artifact" || option.name.toLowerCase().includes("artifact"))
    app.picker.select.setSelectedIndex(index); app.picker.select.selectCurrent()
    await until(`native panel ${phase}`, () => app.outputViewer.visible && setup.captureCharFrame().includes(phase))
  }
  const activate = async (count: number) => {
    await until("source-qualified native action", () => app.outputViewer.actions.visible && app.outputViewer.hint.plainText.includes("Tab actions"))
    setup.mockInput.pressTab(); setup.mockInput.pressEnter()
    await until("native action admission", () => actions.length === count)
    requireThat(actions[count - 1]?.request.action_id === "advance", "wrong native action")
    setup.mockInput.pressEscape(); await until("closed action surface", () => !app.outputViewer.visible)
  }
  let failure: unknown
  try {
    await until("authenticated native driver", client.ready)
    app.composer.value = "/rich-workflow start"; await app.composer.submit()
    await until("actual SDK tool result", () => [...app.transcript.mountedCards.values()].some(row =>
      row.item.content.type === "tool" && row.item.content.name === "rich_artifact" && row.item.content.status.type === "finished"))
    const row = [...app.transcript.mountedCards.values()].find(row => row.item.content.type === "tool" && row.item.content.name === "rich_artifact")!
    requireThat(row.item.content.type === "tool" && row.item.content.status.type === "finished", "canonical rich tool unavailable")
    observations.tool = row.item
    const exactRow = () => {
      const current = [...app.transcript.mountedCards.values()].find(value => value.item.id === row.item.id)
      requireThat(current !== undefined, "canonical native artifact row is no longer mounted")
      requireThat(JSON.stringify(current.item.content) === JSON.stringify(row.item.content), "canonical native artifact changed during actions")
      return current
    }
    const openArtifact = async () => {
      const current = exactRow()
      if (!current.footer.visible) current.toggle()
      await setup.renderOnce()
      await setup.mockMouse.click(current.footer.x + 2, current.footer.y)
    }
    if (!row.presentationFooter.visible) row.toggle()
    await setup.renderOnce()
    await setup.mockMouse.click(row.presentationFooter.x + 2, row.presentationFooter.y)
    await until("SDK rich fields rendered", () => setup.captureCharFrame().includes("Native SDK artifact workflow"))
    await activate(1)
    await panels("advanced:1")
    await activate(2)
    await panels("advanced:2")
    setup.mockInput.pressEscape(); await until("closed native panel", () => !app.outputViewer.visible)
    app.composer.value = "unsent native artifact draft"; app.composer.editor.gotoBufferEnd()
    await control("arm", { mode: "hold" })
    await openArtifact()
    await until("native artifact loading", () => app.outputViewer.visible && app.outputViewer.hint.plainText.includes("Loading"))
    const heldDeadline = performance.now() + 3000
    let held = await control("status")
    while (held.held !== true && held.held !== 1) {
      requireThat(performance.now() < heldDeadline, "actual engine response was not withheld")
      await Bun.sleep(1); held = await control("status")
    }
    observations.held = held
    setup.mockInput.pressEscape(); await until("content dismissed while held", () => !app.outputViewer.visible)
    await setup.mockInput.typeText(" remains responsive")
    await setup.renderOnce()
    requireThat(app.composer.value === "unsent native artifact draft remains responsive", "native artifact request blocked draft input")
    requireThat(setup.captureCharFrame().includes("remains responsive"), "native input acknowledgement missing")
    const retireBy = performance.now() + 3000
    let retired = await control("status")
    while (retired.held !== false && retired.held !== 0) {
      requireThat(performance.now() < retireBy, "cancelled native artifact response retained relay ownership")
      await Bun.sleep(1); retired = await control("status")
    }
    observations.cancelledReadRetired = retired
    await panels("advanced:2")
    setup.mockInput.pressEscape(); await until("panel closed", () => !app.outputViewer.visible)
    await control("release")
    await openArtifact()
    await until("canonical native artifact", () => app.outputViewer.body.plainText.includes("Canonical SDK artifact α.ts:"))
    const firstHeader = app.outputViewer.header.plainText
    setup.mockInput.pressArrow("right")
    await until("second source-qualified native artifact page", () => app.outputViewer.header.plainText !== firstHeader && app.outputViewer.body.plainText.includes("Canonical SDK artifact α.ts:"))
    observations.pageHeaders = [firstHeader, app.outputViewer.header.plainText]
    setup.mockInput.pressEscape(); await until("artifact closed", () => !app.outputViewer.visible)
    await panels("advanced:2")
    const nodes = app.outputViewer.surface.getChildren()
    requireThat(nodes.length > 0, "native panel nodes absent before disconnect")
    await control("disconnect")
    await until("actual disconnect retires native contribution", () => app.state.connection.phase !== "connected" && nodes.every(node => node.isDestroyed))
    requireThat(app.outputViewer.actions.options.length === 0, "disconnected native contribution retained action authority")
    observations.relay = await control("status")
  } catch (error) { failure = error }
  await finishNativeRichProbe(directory, { ...observations, actions }, failure, {
    releaseRelay: async () => { await control("release") },
    closeClient: client.close,
    finalEvidence: () => ({ terminal: client.terminal.snapshot,
      finalAllocationBytes: client.runtime.allocations.usage.bytes }),
  })
}
