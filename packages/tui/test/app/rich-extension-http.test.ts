import { expect, test } from "bun:test"
import { mkdtemp, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { createTestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import { ClientAllocationOwner } from "../../src/client-allocation"
import { EngineHttpSseClient } from "../../src/transport"
import { sessionReader } from "../../src/session-reader-factory"
import { createInitialState } from "../../src/state"
import { MAX_UI_FIELDS, MAX_UI_TABLE_ROWS, PROTOCOL_VERSION, type ClientCommand, type CommandReply, type EngineEvent } from "../../src/protocol"
import { fixturePage, toolItem } from "../fixtures/history"
import { fixturePresentation, surfacePage } from "../fixtures/ui"

// The artifact exists only in the authenticated remote content response. The App
// has a canonical source reference and cannot read a local fixture file for it.
test("native rich contributions retrieve remote source content and retire reload/disconnect authority", async () => {
  const directory = await mkdtemp(join(tmpdir(), "rw-rich-http-"))
  const allocations = new ClientAllocationOwner()
  const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
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
  const item = toolItem(2, "read", "{}", "Artifact preview")
  if (item.content.type !== "tool" || item.content.status.type !== "finished") throw new Error("tool fixture")
  item.content.status.output.complete = false
  item.content.status.presentation = { title: presentation.descriptor.title,
    source: { sequence: "2", selector: { type: "tool_presentation", invocation_id: "invocation-2" } } }
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
        result: { type: "ready", page: { ...fixturePage(command.session_id, command.read), first_ordinal: "0", total_items: "1", items: [item] } } }; break
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
  const app = createRottweilerApp(setup.renderer, { allocations, sessionId: "rich-session", clientId: "rich-client", sessionReader: reader,
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
    expect(setup.captureCharFrame()).toContain("engine.rs")
    setup.mockInput.pressTab(); setup.mockInput.pressEnter()
    await until(() => commands.some(command => command.type === "invoke_ui_action"))
    expect(commands.find(command => command.type === "invoke_ui_action")).toMatchObject({ request: {
      owner: presentation.owner, action_id: "inspect", target: { surface: "tool", invocation_id: "invocation-2" },
    } })
    generation = "b".repeat(32)
    await until(() => app.outputViewer.hint.plainText.includes("unavailable for this extension generation"))
    setup.mockInput.pressTab(); setup.mockInput.pressEnter(); await setup.flush()
    expect(commands.filter(command => command.type === "invoke_ui_action")).toHaveLength(1)
    setup.mockInput.pressEscape(); await until(() => !app.outputViewer.visible)
    hold = true
    const row = app.transcript.mountedCards.get("2")!
    expect(app.outputViewer.visible).toBeFalse()
    await setup.mockMouse.click(row.footer.x + 2, row.footer.y)
    await until(() => pending === 1)
    expect(app.outputViewer.hint.plainText).toContain("Loading")
    setup.mockInput.pressEscape(); await until(() => !app.outputViewer.visible)
    await setup.mockInput.typeText(" while remote read waits")
    expect(app.composer.value).toBe("draft survives while remote read waits")
    expect(pending).toBe(1)
    // A slow remote content read cannot hold native rendering or command input.
    generation = presentation.owner.generation
    await openPanels()
    await until(() => app.outputViewer.surface.getChildren().length === MAX_UI_FIELDS)
    expect(setup.captureCharFrame()).toContain("Artifact section 0")
    await until(() => app.outputViewer.hint.plainText.includes("Tab actions"))
    setup.mockInput.pressTab(); setup.mockInput.pressEnter()
    await until(() => commands.filter(command => command.type === "invoke_ui_action").length === 2)
    expect(commands.filter(command => command.type === "invoke_ui_action")[1]).toMatchObject({ request: {
      owner: panel.owner, contribution_id: "artifact-panel", action_id: "inspect", target: { surface: "panel", revision: 1 },
    } })
    setup.mockInput.pressEscape(); await until(() => !app.outputViewer.visible)
    expect(app.composer.editor.focused).toBeTrue()
    expect(pending).toBe(1)
    release.resolve(); await until(() => pending === 0)
    expect(app.outputViewer.visible).toBeFalse()
    hold = false
    await setup.mockMouse.click(row.footer.x + 2, row.footer.y)
    await until(() => app.outputViewer.body.plainText.includes("Remote canonical artifact: λ.ts"))
    setup.mockInput.pressArrow("right")
    await until(() => artifacts.some(read => read.offset > 0))
    expect(artifacts.every(read => JSON.stringify(read.source) === JSON.stringify({ sequence: "2", selector: { type: "tool_output" } }))).toBeTrue()
    setup.mockInput.pressEscape(); await until(() => !app.outputViewer.visible)
    await openPanels(); await until(() => app.outputViewer.surface.getChildren().length === MAX_UI_FIELDS)
    const nodes = app.outputViewer.surface.getChildren()
    app.resetConnectionProjections()
    expect(nodes.every(node => node.isDestroyed)).toBeTrue()
    expect(app.outputViewer.actions.options).toHaveLength(0)
    malformed = true
    await openPanels()
    await until(() => rejectedReads.includes("get_ui_panels"))
    expect(app.outputViewer.surface.getChildren()).toHaveLength(0)
    expect(app.outputViewer.actions.options).toHaveLength(0)
    expect(commands.filter(command => command.type === "invoke_ui_action")).toHaveLength(2)
  } finally {
    release.resolve(); app.destroy(); await server.stop(true); setup.renderer.destroy()
    const deadline = performance.now() + 2500
    while (allocations.usage.bytes !== 0 && performance.now() < deadline) await Bun.sleep(1)
    if (allocations.usage.bytes !== 0) throw new Error(`Unsettled rich HTTP allocation owner; retained ${directory}`)
    await rm(directory, { recursive: true, force: true })
  }
  expect(allocations.usage.bytes).toBe(0)
}, 10_000)
