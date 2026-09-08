import { writeFile } from "node:fs/promises"
import { join } from "node:path"
import { createMemoryRenderer } from "./memory-renderer"
import { createRottweilerApp } from "../app"
import { ClientAllocationOwner } from "../client-allocation"
import { EngineHttpSseClient } from "../transport"
import { sessionReader } from "../session-reader-factory"
import { createInitialState } from "../state"
import { MAX_UI_FIELDS, MAX_UI_TABLE_ROWS, PROTOCOL_VERSION, type ClientCommand, type CommandReply, type EngineEvent } from "../protocol"
import { mixedHistoryPage } from "./memory-history"
import { fixturePresentation, surfacePage } from "./ui-fixture"

// The artifact exists only in the authenticated remote content response. The App
// has a canonical source reference and cannot read a local fixture file for it.
export async function runRichExtensionProbe(directory: string): Promise<void> {
  const allocations = new ClientAllocationOwner()
  const { setup, treeSitter, terminal } = await createMemoryRenderer()
  const presentation = fixturePresentation()
  presentation.descriptor.actions = [{ id: "inspect", label: "Inspect artifact" }]
  const panel = structuredClone(presentation)
  panel.descriptor.surface = { surface: "panel" }
  panel.descriptor.id = "artifact-panel"
  panel.descriptor.title = "Remote artifact panel"
  // Maximum field and row cardinalities exercise the actual closed native
  // rendering kernel. No extension-supplied renderer callback can enter it.
  panel.descriptor.fields = Array.from({ length: MAX_UI_FIELDS }, (_, index) => ({
    kind: "table", id: `table-${index}`, label: `Artifact section ${index}`, columns: ["Path", "Status"], max_rows: MAX_UI_TABLE_ROWS,
  }))
  panel.projected.fields = panel.descriptor.fields.map(field => ({ kind: "table", id: field.id,
    rows: Array.from({ length: MAX_UI_TABLE_ROWS }, (_, index) => [`remote-${index}.ts`, "Verified"]),
  }))
  const item = mixedHistoryPage("rich-session", { known_view: null, position: { type: "latest" }, max_items: 3, max_bytes: 262144 }, 3, "2").items[2]!
  item.ordinal = "0"
  if (item.content.type !== "tool" || item.content.status.type !== "finished") throw new Error("tool fixture")
  item.content.status.output.text = "Artifact preview"
  item.content.status.output.complete = false
  item.content.status.presentation = { title: presentation.descriptor.title,
    source: { sequence: "2", selector: { type: "tool_presentation", invocation_id: "historical-2" } } }
  let generation = presentation.owner.generation, malformed = false, hold = false, pending = 0, next = 0
  const release = Promise.withResolvers<void>()
  const commands: ClientCommand[] = []
  const rejectedReads: string[] = []
  const artifacts: Array<{ source: unknown; offset: number }> = []
  const server = Bun.serve({ unix: join(directory, "engine.sock"), async fetch(request) {
    if (new URL(request.url).pathname === "/v1/connect") return Response.json({ client_id: "rich-client", token: "remote-token" })
    if (request.headers.get("authorization") !== "Bearer remote-token") return new Response("unauthorized", { status: 401 })
    const command = await request.json() as ClientCommand
    commands.push(command)
    const meta = { ...command.meta, emitted_at: "2026-09-08T00:00:00Z" }
    let event: EngineEvent
    switch (command.type) {
      case "read_transcript": event = { type: "transcript_page_ready", meta, session_id: command.session_id,
        result: { type: "ready", page: { ...mixedHistoryPage(command.session_id, command.read, 1, "2"), first_ordinal: "0", total_items: "1", items: [item] } } }; break
      case "read_session_children": event = { type: "session_children_ready", meta, session_id: command.session_id,
        result: { type: "ready", snapshot: { through: "2", children: [] } } }; break
      case "get_todos": event = { type: "todos_read", meta, session_id: command.session_id,
        result: { type: "ready", todos: { through: "2", snapshot: { items: [] } } } }; break
      case "get_ui_catalog": event = { type: "ui_catalog_ready", meta, session_id: command.session_id,
        catalog: { entries: [presentation, panel].map(value => ({ owner: { ...value.owner, generation }, descriptor: value.descriptor })) } }; break
      case "get_ui_panels": {
        const value = structuredClone(panel)
        if (malformed) (value.descriptor.fields[0] as unknown as { kind: string }).kind = "remote-script-renderer"
        event = { type: "ui_panels_ready", meta, session_id: command.session_id, panels: { panels: [{ revision: 1, presentation: value }] } }; break
      }
      case "read_transcript_content": {
        if (command.read.source.selector.type === "tool_presentation") {
          event = { type: "transcript_content_ready", meta, session_id: command.session_id, page: surfacePage(presentation, command.read) }
        } else {
          artifacts.push({ source: command.read.source, offset: command.read.offset })
          if (hold) { pending++; try { await release.promise } finally { pending-- } }
          const text = "Remote canonical artifact: λ.ts\n".repeat(220)
          const bytes = Buffer.from(text), start = command.read.offset
          let end = Math.min(bytes.length, start + command.read.max_bytes)
          while (end < bytes.length && (bytes[end]! & 0xc0) === 0x80) end--
          event = { type: "transcript_content_ready", meta, session_id: command.session_id,
            page: { view: command.read.view, source: command.read.source, offset: start, next_offset: end < bytes.length ? end : null,
              total_bytes: bytes.length, format: "text", text: bytes.subarray(start, end).toString("utf8") } }
        }
        break
      }
      default: return Response.json({ type: "command", outcome: { type: "accepted" } } satisfies CommandReply)
    }
    return Response.json({ type: "read", outcome: { type: "accepted" }, events: [event] } satisfies CommandReply)
  } })
  const client = new EngineHttpSseClient({ socketPath: join(directory, "engine.sock"), bootstrapToken: "fixture", allocations })
  const reader = sessionReader(async (command, signal, allocation) => {
    try {
      const reply = await client.postCommand(command, signal, allocation)
      if (reply.type !== "read") throw new Error("expected owned read")
      return reply
    } catch (error) { rejectedReads.push(command.type); throw error }
  }, () => ({ protocol_version: PROTOCOL_VERSION, client_id: "rich-client", request_id: `read-${++next}` }))
  const app = createRottweilerApp(setup.renderer, { allocations, treeSitterClient: treeSitter, sessionId: "rich-session", clientId: "rich-client", sessionReader: reader,
    initialState: { ...createInitialState(), driverClientId: "rich-client" },
    onCommand: async (command, allocation) => (await client.postCommand(command, undefined, allocation)).outcome,
  })
  setup.renderer.root.add(app)
  const until = async (ready: () => boolean) => {
    const deadline = performance.now() + 2500
    while (!ready()) { if (performance.now() > deadline) throw new Error(`rich HTTP condition failed: ${app.outputViewer.hint.plainText}`); await Bun.sleep(1); await setup.renderOnce() }
    await setup.renderOnce()
  }
  const openRich = async () => {
    const row = app.transcript.mountedCards.get("2")!
    if (!row.presentationFooter.visible) row.toggle()
    await setup.renderOnce()
    await setup.mockMouse.click(row.presentationFooter.x + 2, row.presentationFooter.y)
    await until(() => app.outputViewer.actions.visible && app.outputViewer.hint.plainText.includes("Tab actions"))
  }
  const openPanels = async () => {
    app.openCommandPicker(); app.commandPalette.selectById("ui.panels"); app.commandPalette.activateSelected()
    await until(() => app.picker.select.options.some(option => option.name === "Remote artifact panel"))
    app.picker.select.selectCurrent()
  }
  try {
    app.composer.value = "draft survives"
    app.composer.editor.gotoBufferEnd()
    await until(() => app.transcript.mountedCards.has("2"))
    await openRich()
    requireThat(setup.captureCharFrame().includes("engine.rs"), "rich tool fields were not rendered")
    setup.mockInput.pressTab(); setup.mockInput.pressEnter()
    await until(() => commands.some(command => command.type === "invoke_ui_action"))
    requireAction(commands, 0, presentation.owner, "result", { surface: "tool", invocation_id: "historical-2" })
    generation = "b".repeat(32)
    await until(() => app.outputViewer.hint.plainText.includes("unavailable for this extension generation"))
    setup.mockInput.pressTab(); setup.mockInput.pressEnter(); await setup.flush()
    requireThat(commands.filter(command => command.type === "invoke_ui_action").length === 1, "retired generation dispatched an action")
    setup.mockInput.pressEscape(); await until(() => !app.outputViewer.visible)
    hold = true
    const row = app.transcript.mountedCards.get("2")!
    requireThat(!app.outputViewer.visible, "closed remote content view reappeared")
    await setup.mockMouse.click(row.footer.x + 2, row.footer.y)
    await until(() => pending === 1)
    requireThat(app.outputViewer.hint.plainText.includes("Loading"), "remote read did not expose loading")
    setup.mockInput.pressEscape(); await until(() => !app.outputViewer.visible)
    await setup.mockInput.typeText(" while remote read waits")
    requireThat(app.composer.value === "draft survives while remote read waits", "held remote read blocked or changed input")
    requireThat(pending === 1, "remote artifact read settled before independent input/render proof")
    // A slow remote content read cannot hold native rendering or command input.
    generation = presentation.owner.generation
    await openPanels()
    await until(() => app.outputViewer.surface.getChildren().length === MAX_UI_FIELDS)
    requireThat(setup.captureCharFrame().includes("Artifact section 0"), "maximum field/row surface was not rendered")
    await until(() => app.outputViewer.hint.plainText.includes("Tab actions"))
    setup.mockInput.pressTab(); setup.mockInput.pressEnter()
    await until(() => commands.filter(command => command.type === "invoke_ui_action").length === 2)
    requireAction(commands, 1, panel.owner, "artifact-panel", { surface: "panel", revision: 1 })
    setup.mockInput.pressEscape(); await until(() => !app.outputViewer.visible)
    requireThat(app.composer.editor.focused, "closing rich panel did not restore composer focus")
    requireThat(pending === 1, "remote artifact read settled before independent input/render proof")
    release.resolve(); await until(() => pending === 0)
    requireThat(!app.outputViewer.visible, "closed remote content view reappeared")
    hold = false
    await setup.mockMouse.click(row.footer.x + 2, row.footer.y)
    await until(() => app.outputViewer.body.plainText.includes("Remote canonical artifact: λ.ts"))
    setup.mockInput.pressArrow("right")
    await until(() => artifacts.some(read => read.offset > 0))
    requireThat(artifacts.every(read => JSON.stringify(read.source) === JSON.stringify({ sequence: "2", selector: { type: "tool_output" } })), "remote artifact lost its canonical source")
    setup.mockInput.pressEscape(); await until(() => !app.outputViewer.visible)
    await openPanels(); await until(() => app.outputViewer.surface.getChildren().length === MAX_UI_FIELDS)
    const nodes = app.outputViewer.surface.getChildren()
    app.resetConnectionProjections()
    requireThat(nodes.every(node => node.isDestroyed), "disconnect retained native panel nodes")
    requireThat(app.outputViewer.actions.options.length === 0, "retired contribution retained action authority")
    malformed = true
    await openPanels()
    await until(() => rejectedReads.includes("get_ui_panels"))
    requireThat(app.outputViewer.surface.getChildren().length === 0, "malformed descriptor reached native rendering")
    requireThat(app.outputViewer.actions.options.length === 0, "retired contribution retained action authority")
    requireThat(commands.filter(command => command.type === "invoke_ui_action").length === 2, "malformed contribution dispatched an action")
  } finally {
    release.resolve(); app.destroy(); await server.stop(true); setup.renderer.destroy()
    const deadline = performance.now() + 2500
    while (allocations.usage.bytes !== 0 && performance.now() < deadline) await Bun.sleep(1)
    if (allocations.usage.bytes !== 0) throw new Error(`Unsettled rich HTTP allocation owner; retained ${directory}`)
  }
  requireThat(allocations.usage.bytes === 0 && terminal.writableLength === 0, "rich HTTP terminal/allocation did not retire")
  await writeFile(join(directory, "rich-http.json"), JSON.stringify({ schemaVersion: 1, pid: process.pid,
    fields: MAX_UI_FIELDS, rows: MAX_UI_TABLE_ROWS, actions: commands.filter(command => command.type === "invoke_ui_action").map(command => command.request),
    artifacts, rejectedReads, finalAllocationBytes: allocations.usage.bytes, terminal: terminal.snapshot,
    oracles: ["rich-tool", "tool-action", "generation-refusal", "input-during-held-read", "native-max-surface", "panel-action", "paged-artifact", "disconnect-destruction", "malformed-rejection"] }) + "\n", { mode: 0o600 })
}

function requireThat(value: unknown, message: string): asserts value { if (!value) throw new Error(message) }
function requireAction(commands: readonly ClientCommand[], index: number, owner: import("../protocol").UiContributionOwner, contribution: string, target: import("../protocol").UiActionRequest["target"]): void {
  const command = commands.filter(command => command.type === "invoke_ui_action")[index]
  requireThat(command !== undefined && command.session_id === "rich-session" && command.request.action_id === "inspect"
    && command.request.contribution_id === contribution && JSON.stringify(command.request.owner) === JSON.stringify(owner)
    && JSON.stringify(command.request.target) === JSON.stringify(target), "rich action source or generation changed")
}
