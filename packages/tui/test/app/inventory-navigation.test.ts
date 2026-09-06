import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import {
  createRottweilerApp
} from "../../src/app"
import type { ClientCommand } from "../../src/protocol"
import { createInitialState } from "../../src/state"
import { emptySessionReader } from "../fixtures/history"

describe("Rottweiler inventory-navigation", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("retries MCP projection failures from the picker", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      onCommand(command) {
        commands.push(command)
        if (command.type === "list_mcp_servers") return Promise.reject(new Error("MCP discovery timed out"))
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openMcpPicker()
    await Bun.sleep(0)
    expect(app.mcpBrowser.itemIds[0]).toBe("mcp.retry")
    expect(app.mcpBrowser.footer.plainText).toContain("MCP discovery timed out")
    app.mcpBrowser.activateSelected()
    await Bun.sleep(0)
    expect(commands.filter((command) => command.type === "list_mcp_servers")).toHaveLength(2)
  })

  test("opens the retained MCP browser and routes inventory actions without changing nested pickers", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        mcpServers: [{
          name: "docs.remote",
          enabled: true,
          approved: true,
          state: { type: "ready" },
          tool_count: 6,
          resource_count: 2,
          prompt_count: 1,
        }],
      },
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openMcpPicker()
    await setup.renderOnce()
    expect(commands.filter((command) => command.type === "list_mcp_servers")).toHaveLength(1)
    expect(app.mcpBrowser.visible).toBeTrue()
    expect(app.picker.visible).toBeFalse()
    expect(app.mcpBrowser).toMatchObject({ x: 0, y: 0, width: 110, height: 27 })
    expect(app.mcpBrowser.listPane.width).toBe(72)
    expect(app.mcpBrowser.divider.x).toBe(73)
    expect(app.mcpBrowser.detailPane.x).toBe(74)

    app.mcpBrowser.selectById("mcp.server.docs.remote")
    expect(app.mcpBrowser.activateSelected()).toBeTrue()
    expect(app.mcpBrowser.visible).toBeFalse()
    expect(app.picker.title).toContain("MCP actions · docs.remote")

    app.closePicker()
    app.openMcpPicker()
    app.mcpBrowser.selectById("mcp.add.http")
    expect(app.mcpBrowser.activateSelected()).toBeTrue()
    expect(app.picker.title).toContain("Add remote MCP server")
  })

  test("keeps cached MCP rows on list failure, retries, and collapses below 108 columns", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    let attempts = 0
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      keybindings: { preset: "vim" },
      initialState: {
        ...createInitialState(),
        mcpServers: [{
          name: "broken.remote",
          enabled: true,
          approved: true,
          state: { type: "failed", message: "TLS certificate rejected" },
          tool_count: 2,
          resource_count: 0,
          prompt_count: 0,
        }],
      },
      onCommand(command) {
        if (command.type === "list_mcp_servers") {
          attempts += 1
          return {
            type: "rejected",
            error: { category: "protocol", code: "mcp_unavailable", message: "MCP discovery timed out", retryable: true },
          }
        }
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openMcpPicker()
    await Bun.sleep(0)
    await setup.renderOnce()
    expect(app.mcpBrowser.itemIds).toContain("mcp.server.broken.remote")
    expect(app.mcpBrowser.itemIds).toContain("mcp.retry")
    expect(app.mcpBrowser.footer.plainText).toContain("MCP discovery timed out")
    setup.mockInput.pressKey("r", { ctrl: true })
    await Bun.sleep(0)
    expect(attempts).toBe(2)

    app.mcpBrowser.selectById("mcp.server.broken.remote")
    setup.resize(107, 18)
    await setup.renderOnce()
    await setup.renderOnce()
    expect(app.mcpBrowser.layoutMode).toBe("single")
    expect(app.mcpBrowser.listPane.width).toBe(105)
    expect(app.mcpBrowser.divider.visible).toBeFalse()
    expect(app.mcpBrowser.compactDetail.plainText).toContain("TLS certificate rejected")
    expect(app.mcpBrowser.footer.plainText).toContain("Esc×2 close")

    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.mcpBrowser.visible).toBeTrue()
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.mcpBrowser.visible).toBeFalse()
  })

  test("returns from every nested MCP picker to the retained inventory", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        mcpServers: [{
          name: "docs.remote",
          enabled: true,
          approved: true,
          state: { type: "ready" },
          tool_count: 6,
          resource_count: 2,
          prompt_count: 1,
        }],
      },
    })
    renderer.root.add(app)

    const expectRestored = (): void => {
      expect(app.mcpBrowser.visible).toBeTrue()
      expect(app.mcpBrowser.input.focused).toBeTrue()
      expect(app.mcpBrowser.input.value).toBe("docs")
      expect(app.mcpBrowser.selectedId).toBe("mcp.server.docs.remote")
    }

    app.openMcpPicker()
    await setup.mockInput.typeText("docs")
    app.mcpBrowser.selectById("mcp.server.docs.remote")
    app.mcpBrowser.activateSelected()
    await setup.renderOnce()
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expectRestored()

    app.mcpBrowser.close()
    app.openMcpPicker()
    await setup.mockInput.typeText("docs")
    app.mcpBrowser.selectById("mcp.server.docs.remote")
    app.mcpBrowser.activateSelected()
    const remove = app.picker.select.options.findIndex((option) => option.value === "mcp.remove.docs.remote")
    app.picker.select.setSelectedIndex(remove)
    app.picker.select.selectCurrent()
    await setup.renderOnce()
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expectRestored()

    app.mcpBrowser.close()
    app.openMcpPicker()
    app.mcpBrowser.selectById("mcp.add.http")
    app.mcpBrowser.activateSelected()
    await setup.renderOnce()
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.mcpBrowser.visible).toBeTrue()
    expect(app.mcpBrowser.input.focused).toBeTrue()
    expect(app.mcpBrowser.selectedId).toBe("mcp.add.http")
  })

  test("clears a partial anchored trigger before opening a local slash action", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        models: [{ id: "openai/fast", displayName: "fast", provider: "openai", aliases: ["fast"], current: false, available: true, status: null, vision: true, thinking: true, toolCalling: true }],
      },
    })
    renderer.root.add(app)
    await setup.mockInput.typeText("/model")
    app.picker.select.selectCurrent()
    await setup.renderOnce()

    expect(app.composer.value).toBe("")
    expect(app.picker.title).toContain("Models")
  })

  test("scrolls the Ctrl-P viewport without moving selection and activates the exact mouse row", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        commands: Array.from({ length: 12 }, (_, index) => ({
          name: `command-${index}`,
          description: `Command ${index}`,
          usage: `/command-${index}`,
        })),
      },
    })
    renderer.root.add(app)
    app.openCommandPicker()
    await setup.renderOnce()

    expect(app.commandPalette.selectedId).toBe("compact.run")
    expect(app.commandPalette.scrollOffset).toBe(0)
    await setup.mockMouse.scroll(app.commandPalette.listPane.x + 2, app.commandPalette.listPane.y + 1, "down")
    expect(app.commandPalette.selectedId).toBe("compact.run")
    expect(app.commandPalette.scrollOffset).toBe(1)
    await setup.mockMouse.click(app.commandPalette.listPane.x + 2, app.commandPalette.listPane.y)
    expect(app.picker.visible).toBeTrue()
    expect(app.picker.title).toContain("Commands")
    expect(app.composer.value).toBe("/compact")
  })

  test("centers Ctrl-P keyboard selection instead of following viewport edges", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        commands: Array.from({ length: 30 }, (_, index) => ({
          name: `command-${index}`,
          description: `Command ${index}`,
          usage: `/command-${index}`,
        })),
      },
    })
    renderer.root.add(app)
    app.openCommandPicker()
    await setup.renderOnce()

    const visible = app.commandPalette.visibleRowCount
    const maximum = app.commandPalette.rowCount - visible
    for (let index = 1; index <= visible + 2; index += 1) {
      setup.mockInput.pressArrow("down")
      const selected = index + 1
      expect(app.commandPalette.selectedRowIndex).toBe(selected)
      expect(app.commandPalette.scrollOffset).toBe(Math.min(maximum, Math.max(0, selected - Math.floor(visible / 2))))
    }
    setup.mockInput.pressArrow("up")
    const previous = visible + 2
    expect(app.commandPalette.selectedRowIndex).toBe(previous)
    expect(app.commandPalette.scrollOffset).toBe(
      Math.min(maximum, Math.max(0, previous - Math.floor(visible / 2))),
    )
    setup.mockInput.pressKey("HOME")
    expect(app.commandPalette.scrollOffset).toBe(0)
    setup.mockInput.pressKey("END")
    expect(app.commandPalette.scrollOffset).toBe(maximum)
  })
})
