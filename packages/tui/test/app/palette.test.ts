import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import {
  createRottweilerApp
} from "../../src/app"
import type { CommandOutcome } from "../../src/protocol"
import { createInitialState } from "../../src/state"
import { emptySessionReader } from "../fixtures/history"

describe("Rottweiler palette", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("keeps local slash actions and the full action palette useful before engine projections", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader })
    renderer.root.add(app)

    await setup.mockInput.typeText("/")
    const slash = app.picker.select.options.map((option) => option.value)
    expect(slash).toContain("providers")
    expect(slash).toContain("agents")
    expect(slash).toContain("theme")
    expect(slash).toContain("settings")
    expect(slash).toContain("exit")
    expect(slash).not.toContain("help")
    expect(slash).not.toContain("status")

    app.closePicker()
    app.openCommandPicker()
    const palette = app.commandPalette.itemIds
    expect(palette).toContain("session.list")
    expect(palette).toContain("provider.list")
    expect(palette).toContain("agent.children")
    expect(palette).toContain("mcp.manage")
    expect(palette).toContain("keyboard.help")
    expect(palette).not.toContain("mcp.configure")
    expect(palette).toContain("permissions.manage")
    expect(palette.length).toBeGreaterThan(10)

    app.commandPalette.selectById("status.show")
    app.commandPalette.activateSelected()
    expect(app.composer.value).toBe("/status")
  })

  test("opens the command palette as a split list and selected-only detail surface", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader })
    renderer.root.add(app)
    app.openCommandPicker()
    await setup.renderOnce()

    expect(app.picker.visible).toBeFalse()
    expect(app.commandPalette.visible).toBeTrue()
    expect(app.commandPalette.x).toBe(1)
    expect(app.commandPalette.y).toBe(2)
    expect(app.commandPalette.width).toBe(108)
    expect(app.commandPalette.height).toBe(25)
    expect(app.commandPalette.detail.plainText).toContain("Compact the conversation context")
    expect(app.commandPalette.footer.plainText).toContain("built-in")

    await setup.mockInput.typeText("status")
    expect(app.commandPalette.detail.plainText).toContain("Display running and queue state")
    expect(renderer.currentFocusedRenderable).toBe(app.commandPalette.input)
    setup.mockInput.pressEnter()
    expect(app.commandPalette.visible).toBeFalse()
    expect(app.composer.value).toBe("/status")
  })

  test("keeps local command palette actions usable while the live catalog loads", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const pending = new Promise<CommandOutcome>(() => { })
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      onCommand(command) {
        return command.type === "list_commands" ? pending : { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.openCommandPicker()
    await setup.renderOnce()

    expect(app.commandPalette.footer.plainText).toContain("Loading extension commands")
    await setup.mockInput.typeText("workspace roots")
    expect(app.commandPalette.detail.plainText).toContain("See every live workspace root")
  })

  test("retries a failed command catalog from the command palette", async () => {
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
          error: {
            category: "protocol",
            code: "catalog_unavailable",
            message: "driver lease rejected the command catalog",
            retryable: true,
          },
        }
      },
    })
    renderer.root.add(app)
    app.openCommandPicker()
    await Bun.sleep(0)

    expect(app.commandPalette.footer.plainText).toContain("driver lease rejected the command catalog")
    expect(attempts).toBe(1)
    setup.mockInput.pressKey("r", { ctrl: true })
    await Bun.sleep(0)
    expect(attempts).toBe(2)
  })

  test("derives command palette source counts and truncation from the live catalog", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        commands: [{ name: "deploy", description: "Deploy project", usage: "/deploy", source: "project" }],
        commandsTruncated: true,
      },
    })
    renderer.root.add(app)
    app.openCommandPicker()

    expect(app.commandPalette.footer.plainText).toContain("1 extension")
    expect(app.commandPalette.footer.plainText).toContain("results are truncated")
  })

  test("preserves local, prefill, open, and live dispatch from the command palette", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        commands: [{ name: "deploy", description: "Deploy project", usage: "/deploy" }],
      },
    })
    renderer.root.add(app)

    app.openCommandPicker()
    await setup.mockInput.typeText("add workspace directory")
    setup.mockInput.pressEnter()
    expect(app.composer.value).toBe("/add-dir ")

    app.composer.value = ""
    app.openCommandPicker()
    await setup.mockInput.typeText("switch model")
    setup.mockInput.pressEnter()
    expect(app.picker.visible).toBeTrue()
    expect(app.picker.title).toContain("Models")

    app.closePicker()
    app.openCommandPicker()
    await setup.mockInput.typeText("/deploy")
    setup.mockInput.pressEnter()
    expect(app.composer.value).toBe("/deploy")
  })

  test("groups an empty palette in fixed section order and removes headers while filtering", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        commands: [{
          name: "deploy",
          description: "Deploy the project",
          usage: "/deploy [environment]",
          source: "project",
        }],
      },
    })
    renderer.root.add(app)
    app.openCommandPicker()

    const headers = app.commandPalette.sectionLabels
    expect(headers).toEqual([
      "Conversation",
      "Agents & models",
      "Workspace",
      "Safety",
      "Appearance & settings",
      "Help & system",
      "Commands",
    ])
    expect(app.commandPalette.itemIds).not.toContain("interrupt.run")
    expect(app.commandPalette.selectedId).toBe("compact.run")

    await setup.mockInput.typeText("model")
    expect(app.commandPalette.sectionLabels).toEqual([])
    expect(app.commandPalette.itemIds).toContain("model.list")
  })

  test("lists searchable keyboard shortcuts from the active compiled bindings", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      keybindings: {
        bindings: { global: { open_model_picker: "ctrl+k" } },
      },
    })
    renderer.root.add(app)
    app.openKeyboardHelpPicker()

    expect(app.picker.title).toContain("Keyboard shortcuts")
    expect(app.picker.select.options
      .filter((option) => String(option.value).startsWith("keyboard-help.section."))
      .map((option) => option.description)).toEqual(["Global", "Editing", "Review"])
    const model = app.picker.select.options.find(
      (option) => option.description === "Switch model",
    )
    expect(model?.name).toBe("Ctrl+K")
    expect(app.picker.select.options.find(
      (option) => option.description === "Select previous block",
    )?.name).toBe("Ctrl+UP")
    expect(app.picker.select.options.find(
      (option) => option.description === "Select next block",
    )?.name).toBe("Ctrl+DOWN")
    expect(app.picker.select.options.find(
      (option) => option.description === "Expand or collapse block",
    )?.name).toBe("Ctrl+Space")

    await setup.mockInput.typeText("switch model")
    expect(app.picker.select.options.some(
      (option) => String(option.value).startsWith("keyboard-help.section."),
    )).toBeFalse()
    expect(app.picker.select.options.map((option) => option.name)).toContain("Ctrl+K")

    app.closePicker()
    app.openKeyboardHelpPicker()
    await setup.mockInput.typeText("ctrl+k")
    expect(app.picker.select.options.map((option) => option.name)).toContain("Ctrl+K")
    app.picker.select.selectCurrent()
    expect(app.picker.visible).toBeFalse()
  })
})
