import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { createRottweilerApp } from "../../src/app"
import { MCP_ENVIRONMENT_DRAFT_LIMITS } from "../../src/app/mcp-draft"
import { PROTOCOL_VERSION, type ClientCommand } from "../../src/protocol"
import { emptyHistoryReader } from "../fixtures/history"

describe("MCP draft interaction", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => { renderer?.destroy(); renderer = undefined })

  test("empty argument/environment fields submit directly and overflowing UTF-8 entries stay outside the command", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      onCommand(command) { commands.push(command); return { type: "accepted" } },
    })
    renderer.root.add(app)
    app.openMcpPicker()
    const list = commands.at(-1)
    if (list?.type !== "list_mcp_servers") throw new Error("missing MCP query")
    app.handleEvent({
      type: "mcp_servers_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "tui", request_id: list.meta.request_id, emitted_at: "2026-01-01T00:00:00Z" },
      session_id: "session-local", servers: [],
    })
    app.mcpBrowser.selectById("mcp.add.stdio")
    app.mcpBrowser.activateSelected()
    await setup.mockInput.typeText("docs")
    setup.mockInput.pressEnter()
    await setup.mockInput.typeText("/usr/local/bin/docs-mcp")
    setup.mockInput.pressEnter()
    expect(app.picker.input.value).toBe("")
    setup.mockInput.pressEnter()
    expect(app.picker.input.value).toBe("")
    for (let i = 0; i < 12; i += 1) {
      const value = `K${i}=${"界".repeat(2_000)}`
      app.picker.input.value = value
      expect(app.picker.input.value).toBe(value)
      setup.mockInput.pressEnter()
    }
    expect(app.state.errors.at(-1)?.code).toBe("mcp_environment_full")
    expect(app.picker.input.value).toBe("")
    setup.mockInput.pressEnter()
    const submitted = commands.find(command => command.type === "add_mcp_stdio_server")
    if (submitted?.type !== "add_mcp_stdio_server") throw new Error("missing MCP submission")
    expect(submitted.args).toEqual([])
    expect(submitted.environment).toHaveLength(10)
    expect(submitted.environment.reduce((sum, entry) => sum + Buffer.byteLength(entry.key) + Buffer.byteLength(entry.value), 0))
      .toBeLessThanOrEqual(MCP_ENVIRONMENT_DRAFT_LIMITS.bytes)
    expect(app.picker.input.value).toBe("")
  })

  test("switching sessions clears an unfinished environment draft and its modal", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      onCommand(command) { commands.push(command); return { type: "accepted" } },
    })
    renderer.root.add(app)
    app.openMcpPicker()
    const list = commands.at(-1)
    if (list?.type !== "list_mcp_servers") throw new Error("missing MCP query")
    app.handleEvent({
      type: "mcp_servers_listed",
      meta: { protocol_version: PROTOCOL_VERSION, client_id: "tui", request_id: list.meta.request_id, emitted_at: "2026-01-01T00:00:00Z" },
      session_id: "session-local", servers: [],
    })
    app.mcpBrowser.selectById("mcp.add.stdio")
    app.mcpBrowser.activateSelected()
    for (const value of ["docs", "/usr/local/bin/docs-mcp", "", "TOKEN=private-draft"]) {
      app.picker.input.value = value
      setup.mockInput.pressEnter()
    }
    app.picker.input.value = "UNFINISHED=private-input"
    app.setSessionId("next-session")
    expect(app.picker.visible).toBe(false)
    expect(app.mcpBrowser.visible).toBe(false)
    expect(app.picker.input.value).toBe("")
    expect(JSON.stringify(app.state)).not.toContain("private-")
    expect(commands.some(command => command.type === "add_mcp_stdio_server")).toBe(false)
  })

})
