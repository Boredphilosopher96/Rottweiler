import { readyCatalog } from "../fixtures/catalog"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { createRottweilerApp } from "../../src/app"
import type { ClientCommand } from "../../src/protocol"
import { createInitialState, type RottweilerState } from "../../src/state"
import { emptySessionReader } from "../fixtures/history"
import { options } from "../picker-screen"

const REMOVED = [
  "goto", "status", "interrupt", "plan", "fork", "trust", "add-dir", "cost", "models", "providers",
  "deep-init", "workflow-status", "mcp.prompt",
]

const childState = (): RottweilerState => ({
  ...createInitialState(), subagentOrder: ["child"], subagents: { child: {
    projectionId: "child", subagentId: "child", parentTurnId: "1", task: "Inspect code", spawnedAtMs: null,
    status: "completed", childSessionId: "child-session", lastChildSequence: "4", activity: null,
    summary: "Done.", touchedFileCount: 0, diffArtifactId: null, cost: { kind: "monetary", currency: "USD", amount_micros: "0" },
  } },
})

describe("Rottweiler palette", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("lists one engine catalog entry per action in fixed sections before engine projections", async () => {
    const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader })
    renderer.root.add(app)
    app.openCommandPicker()

    expect(app.commandPalette.sectionLabels).toEqual([
      "Conversation", "Models & agents", "Context & usage", "Workspace", "Safety", "Settings & help",
    ])
    const ids = app.commandPalette.itemIds
    expect(new Set(ids).size).toBe(ids.length)
    for (const id of ["cmd.new", "cmd.resume", "cmd.rewind", "cmd.compact", "cmd.model", "cmd.mode", "cmd.context",
      "cmd.usage", "cmd.review", "cmd.dirs", "cmd.mcp", "cmd.init", "cmd.memory", "cmd.permissions", "cmd.settings",
      "cmd.theme", "cmd.help", "cmd.exit"]) expect(ids).toContain(id)
    // Agents, queued messages, and errors appear only once they exist.
    for (const id of ["cmd.agents", "cmd.queue", "cmd.errors"]) expect(ids).not.toContain(id)
    for (const name of REMOVED) {
      expect(ids).not.toContain(`cmd.${name}`)
      expect(ids).not.toContain(`ext.${name}`)
    }
    expect(app.commandPalette.selectedId).toBe("cmd.new")
  })

  test("shows the Agents entry once this session has a child agent", async () => {
    const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, initialState: childState() })
    renderer.root.add(app)
    app.openCommandPicker()
    expect(app.commandPalette.itemIds).toContain("cmd.agents")
  })

  test("renders a split list with right-aligned keys and toggles closed with Ctrl+P", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader })
    renderer.root.add(app)
    setup.mockInput.pressKey("p", { ctrl: true })
    await setup.renderOnce()

    expect(app.commandPalette.visible).toBeTrue()
    expect(app.commandPalette.x).toBe(0)
    expect(app.commandPalette.y).toBe(0)
    expect(app.commandPalette.width).toBe(110)
    expect(app.commandPalette.height).toBe(app.composer.y)
    const frame = setup.captureCharFrame()
    expect(frame).toMatch(/New session\s+Ctrl\+N/)
    expect(frame).toMatch(/Review changes\s+Ctrl\+R/)
    expect(app.commandPalette.footer.plainText).toContain("Ctrl+P close")
    expect(app.commandPalette.detail.plainText).toContain("/new")

    await setup.mockInput.typeText("providers")
    expect(app.commandPalette.selectedId).toBe("cmd.model")
    expect(renderer.currentFocusedRenderable).toBe(app.commandPalette.input)
    setup.mockInput.pressKey("p", { ctrl: true })
    expect(app.commandPalette.visible).toBeFalse()
  })

  test("labels extension commands by source and dispatches them", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        connection: { phase: "connected", attempt: 0, error: null, gap: null },
        commands: [
          { name: "deploy", description: "Deploy project", usage: "/deploy <environment>", source: "project" },
          { name: "careful", description: "Warn before destructive commands", usage: "/careful", source: "skill" },
          { name: "mcp.github.triage", description: "MCP prompt triage from github", usage: "/mcp.github.triage [JSON object]", source: "mcp" },
          { name: "compact", description: "Compact conversation context", usage: "/compact [instructions]", source: "builtin" },
        ],
        commandsTruncated: true,
      },
      onCommand(command) { emitted.push(command); return { type: "accepted" } },
    })
    renderer.root.add(app)
    app.openCommandPicker()
    await setup.renderOnce()

    expect(app.commandPalette.sectionLabels.at(-1)).toBe("Extensions")
    expect(app.commandPalette.itemIds.filter((id) => id.startsWith("ext."))).toEqual([
      "ext.careful", "ext.deploy", "ext.mcp.github.triage",
    ])
    expect(app.commandPalette.itemIds.filter((id) => id.endsWith(".compact"))).toEqual(["cmd.compact"])
    expect(app.commandPalette.footer.plainText).toContain("Extension results are truncated")
    await setup.mockInput.typeText("triage")
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("mcp · github")
    setup.mockInput.pressEnter()
    await Bun.sleep(0)
    expect(emitted.at(-1)).toEqual(expect.objectContaining({ type: "send_message", content: "/mcp.github.triage" }))

    app.openCommandPicker()
    await setup.mockInput.typeText("deploy")
    setup.mockInput.pressEnter()
    expect(app.composer.value).toBe("/deploy ")

    app.composer.value = ""
    app.openCommandPicker()
    await setup.mockInput.typeText("careful")
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toMatch(/careful\s+skill/)
  })

  test("sinks engine-refused actions with their reason and never selects them first", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        availableActions: [
          { action: "rewind", unavailable_reason: "Stop the current turn or wait for it to finish." },
          { action: "compact", queued: true, unavailable_reason: null },
        ],
      },
    })
    renderer.root.add(app)
    app.openCommandPicker()

    const ids = app.commandPalette.itemIds
    const conversation = ids.slice(0, ids.indexOf("cmd.model"))
    expect(conversation.at(-1)).toBe("cmd.rewind")
    app.commandPalette.selectById("cmd.rewind")
    expect(app.commandPalette.detail.plainText).toContain("Stop the current turn")
    expect(app.commandPalette.activateSelected()).toBeFalse()
    app.commandPalette.selectById("cmd.compact")
    expect(app.commandPalette.detail.plainText).toContain("Queues until the current work finishes")
    await setup.mockInput.typeText("rewind")
    expect(app.commandPalette.itemIds).toEqual(["cmd.rewind"])
    expect(app.commandPalette.activateSelected()).toBeFalse()
  })

  test("opens screens from the palette and lists recent commands first", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, onCommand: readyCatalog(() => app) })
    renderer.root.add(app)

    app.openCommandPicker()
    await setup.mockInput.typeText("model")
    setup.mockInput.pressEnter()
    expect(app.picker.visible).toBeTrue()
    expect(app.picker.screenTitle).toContain("MODELS")
    app.closePicker()

    app.openCommandPicker()
    await setup.mockInput.typeText("dirs")
    setup.mockInput.pressEnter()
    expect(app.picker.visible).toBeTrue()
    app.closePicker()

    app.openCommandPicker()
    expect(app.commandPalette.sectionLabels[0]).toBe("Recent")
    expect(app.commandPalette.itemIds.slice(0, 2)).toEqual(["cmd.dirs", "cmd.model"])
    expect(app.commandPalette.itemIds.filter((id) => id === "cmd.model")).toHaveLength(1)
  })

  test("offers a retry row when the live catalog fails", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    let attempts = 0
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      onCommand(command) {
        if (command.type !== "list_commands") return { type: "accepted" }
        attempts += 1
        return {
          type: "rejected",
          error: { category: "protocol", code: "catalog_unavailable", message: "driver lease rejected the command catalog", retryable: true },
        }
      },
    })
    renderer.root.add(app)
    app.openCommandPicker()
    await Bun.sleep(0)

    expect(app.commandPalette.footer.plainText).toContain("driver lease rejected the command catalog")
    expect(attempts).toBe(1)
    app.commandPalette.selectById("ext.retry")
    app.commandPalette.activateSelected()
    await Bun.sleep(0)
    expect(attempts).toBe(2)
    expect(app.commandPalette.visible).toBeTrue()
  })

  test("help lists commands and the active compiled key bindings", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      keybindings: { bindings: { global: { open_model_picker: "ctrl+k" } } },
    })
    renderer.root.add(app)
    await setup.mockInput.typeText("?")
    expect(app.composer.value).toBe("")
    expect(app.picker.screenTitle).toContain("HELP")
    expect(app.picker.sectionLabels).toEqual([
      "Conversation", "Models & agents", "Context & usage", "Workspace", "Safety", "Settings & help",
      "Keys · Global", "Keys · Editing", "Keys · Review",
    ])
    const model = app.picker.items.find((item) => item.label === "Switch model")
    expect(model?.hint).toBe("Ctrl+K")
    expect(model?.primary).toBeNull()
    expect(options(app.picker).some((option) => option.name === "/compact [instructions]")).toBeTrue()
    expect(app.picker.items.some((item) => item.hint === "Ctrl+O")).toBeFalse()
    expect(app.picker.items.find((item) => item.label.startsWith("Switch between conversation"))?.hint).toBe("Ctrl+T")
  })
})
