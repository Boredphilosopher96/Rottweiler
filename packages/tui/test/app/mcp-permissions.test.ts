import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import {
  createRottweilerApp
} from "../../src/app"
import type { ClientCommand } from "../../src/protocol"
import { emptyHistoryReader } from "../fixtures/history"

describe("Rottweiler mcp-permissions", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("reviews, confirms, and enables a live MCP server through typed commands", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.openMcpPicker()
    expect(emitted.at(-1)).toEqual(expect.objectContaining({ type: "list_mcp_servers" }))
    const list = emitted.at(-1)
    if (list?.type !== "list_mcp_servers") throw new Error("missing MCP server list")
    app.handleEvent({
      type: "mcp_servers_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui",
        request_id: list.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      servers: [],
    })
    expect(app.mcpBrowser.itemIds).toEqual([
      "mcp.add.http",
      "mcp.add.stdio",
    ])
    app.mcpBrowser.selectById("mcp.add.http")
    app.mcpBrowser.activateSelected()
    await setup.mockInput.typeText("docs.remote")
    setup.mockInput.pressEnter()
    await setup.mockInput.typeText("https://example.com/mcp")
    setup.mockInput.pressEnter()
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "add_mcp_http_server",
      name: "docs.remote",
      endpoint: "https://example.com/mcp",
    }))
    const refreshedList = emitted.findLast(
      (command) => command.type === "list_mcp_servers",
    )
    if (refreshedList?.type !== "list_mcp_servers") {
      throw new Error("missing refreshed MCP server list")
    }

    app.handleEvent({
      type: "mcp_servers_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui",
        request_id: refreshedList.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      servers: [{
        name: "docs.remote",
        enabled: false,
        approved: false,
        state: { type: "approval_required" },
        tool_count: 0,
        resource_count: 0,
        prompt_count: 0,
      }],
    })
    expect(app.mcpBrowser.itemIds).toContain("mcp.server.docs.remote")
    app.mcpBrowser.selectById("mcp.server.docs.remote")
    expect(app.mcpBrowser.detail.plainText).toContain("Approval needed")
    expect(app.mcpBrowser.detail.plainText).not.toContain("approval_required")
    app.mcpBrowser.activateSelected()
    expect(app.picker.title).toContain("MCP actions · docs.remote")
    expect(app.picker.select.options.map((option) => option.name)).toEqual([
      "Enable",
      "Review fingerprint",
      "Remove",
    ])
    const reviewIndex = app.picker.select.options.findIndex(
      (option) => option.value === "mcp.review.docs.remote",
    )
    app.picker.select.setSelectedIndex(reviewIndex)
    app.picker.select.selectCurrent()
    expect(emitted.at(-1)).toEqual(expect.objectContaining({
      type: "review_mcp_server",
      name: "docs.remote",
    }))
    const reviewCommand = emitted.at(-1)
    if (reviewCommand?.type !== "review_mcp_server") {
      throw new Error("missing MCP review command")
    }

    const fingerprint = "a".repeat(64)
    app.handleEvent({
      type: "mcp_server_approval_reviewed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui",
        request_id: reviewCommand.meta.request_id,
        emitted_at: "2026-01-01T00:00:01Z",
      },
      session_id: "session-local",
      review: {
        server: "docs.remote",
        transport: "streamable_http",
        endpoint: "https://example.com/mcp",
        origin: "user",
        defer_tools: true,
        fingerprint,
        previously_approved: false,
      },
    })
    const approveIndex = app.picker.select.options.findIndex(
      (option) => option.value === "mcp.approve.docs.remote",
    )
    expect(app.picker.select.options[approveIndex]?.description).toContain(fingerprint)
    expect(app.picker.select.options[approveIndex]?.description).toContain("Remote HTTPS")
    expect(app.picker.select.options[approveIndex]?.description).not.toContain("streamable_http")
    app.picker.select.setSelectedIndex(approveIndex)
    app.picker.select.selectCurrent()
    expect(emitted.at(-1)).toEqual(expect.objectContaining({
      type: "approve_mcp_server",
      name: "docs.remote",
      fingerprint,
    }))
    const approveCommand = emitted.at(-1)
    if (approveCommand?.type !== "approve_mcp_server") {
      throw new Error("missing MCP approve command")
    }

    app.handleEvent({
      type: "mcp_servers_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui",
        request_id: approveCommand.meta.request_id,
        emitted_at: "2026-01-01T00:00:02Z",
      },
      session_id: "session-local",
      servers: [{
        name: "docs.remote",
        enabled: false,
        approved: true,
        state: { type: "disabled" },
        tool_count: 0,
        resource_count: 0,
        prompt_count: 0,
      }],
    })
    const enableIndex = app.picker.select.options.findIndex(
      (option) => option.value === "mcp.toggle.docs.remote",
    )
    app.picker.select.setSelectedIndex(enableIndex)
    app.picker.select.selectCurrent()
    expect(emitted.at(-1)).toEqual(expect.objectContaining({
      type: "set_mcp_server_enabled",
      name: "docs.remote",
      enabled: true,
    }))
    const enableCommand = emitted.at(-1)
    if (enableCommand?.type !== "set_mcp_server_enabled") {
      throw new Error("missing MCP enable command")
    }

    app.handleEvent({
      type: "mcp_servers_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui",
        request_id: enableCommand.meta.request_id,
        emitted_at: "2026-01-01T00:00:03Z",
      },
      session_id: "session-local",
      servers: [{
        name: "docs.remote",
        enabled: true,
        approved: true,
        state: { type: "disabled" },
        tool_count: 0,
        resource_count: 0,
        prompt_count: 0,
      }],
    })
    const connectIndex = app.picker.select.options.findIndex(
      (option) => option.value === "mcp.toggle.docs.remote",
    )
    expect(app.picker.select.options[connectIndex]?.name).toBe("Enable")
    app.picker.select.setSelectedIndex(connectIndex)
    app.picker.select.selectCurrent()
    expect(emitted.at(-1)).toEqual(expect.objectContaining({
      type: "set_mcp_server_enabled",
      name: "docs.remote",
      enabled: true,
    }))
    const removeIndex = app.picker.select.options.findIndex(
      (option) => option.value === "mcp.remove.docs.remote",
    )
    app.picker.select.setSelectedIndex(removeIndex)
    app.picker.select.selectCurrent()
    expect((app.picker.title ?? "").trim()).toBe("Remove docs.remote? This deletes its configuration")
    expect(app.picker.select.options.map((option) => option.name)).toEqual(["Remove", "Cancel"])
    app.picker.select.setSelectedIndex(0)
    app.picker.select.selectCurrent()
    expect(emitted.at(-1)).toEqual(expect.objectContaining({
      type: "remove_mcp_server",
      name: "docs.remote",
    }))
    expect(emitted.some((command) => command.type === "send_message")).toBe(false)
  })

  test("builds a redacted stdio MCP command through the full prompt chain", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.openMcpPicker()
    const list = emitted.at(-1)
    if (list?.type !== "list_mcp_servers") throw new Error("missing MCP server list")
    app.handleEvent({
      type: "mcp_servers_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui",
        request_id: list.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      servers: [],
    })
    app.mcpBrowser.selectById("mcp.add.stdio")
    app.mcpBrowser.activateSelected()
    expect((app.picker.title ?? "").trim()).toBe("Server name, e.g. docs")
    await setup.mockInput.typeText("docs")
    setup.mockInput.pressEnter()
    expect((app.picker.title ?? "").trim()).toBe("Executable path, e.g. /usr/local/bin/docs-mcp")
    await setup.mockInput.typeText("/usr/local/bin/docs-mcp")
    setup.mockInput.pressEnter()
    expect((app.picker.title ?? "").trim()).toBe(
      "Arguments separated by spaces · quoting is not supported · leave empty for none",
    )
    await setup.mockInput.typeText("--stdio   docs")
    setup.mockInput.pressEnter()
    expect((app.picker.title ?? "").trim()).toBe(
      "Environment variable as KEY=VALUE · leave empty to finish",
    )
    await setup.mockInput.typeText("missing-separator")
    setup.mockInput.pressEnter()
    expect(
      emitted.some((command) => command.type === "add_mcp_stdio_server"),
    ).toBeFalse()
    expect((app.picker.title ?? "").trim()).toBe(
      "Environment variable as KEY=VALUE · leave empty to finish",
    )
    const secret = "secret-canary=value"
    await setup.mockInput.typeText(`DOCS_TOKEN=${secret}`)
    setup.mockInput.pressEnter()
    expect((app.picker.title ?? "").trim()).toBe(
      "Environment variable as KEY=VALUE · leave empty to finish",
    )
    setup.mockInput.pressEnter()

    expect(emitted).toContainEqual(expect.objectContaining({
      type: "add_mcp_stdio_server",
      name: "docs",
      executable: "/usr/local/bin/docs-mcp",
      args: ["--stdio", "docs"],
      environment: [{ key: "DOCS_TOKEN", value: secret }],
    }))
    const visiblePickerCopy = app.picker.select.options
      .flatMap((option) => [option.name, option.description])
      .join("\n")
    expect(visiblePickerCopy).not.toContain(secret)
    expect(app.statusLine.plainText).not.toContain(secret)
  })

  test("keeps MCP management inert in replay sessions", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      replaySessionId: "historical-session",
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.openMcpPicker()
    expect(emitted).toEqual([])
    expect(app.picker.visible).toBeFalse()
    expect(app.mcpBrowser.visible).toBeFalse()
  })

  test("manages typed permission rows without transcript JSON or manual ids", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(setup.renderer, {
      historyReader: emptyHistoryReader,
      sessionId: "session-permissions",
      clientId: "permission-driver",
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    setup.renderer.root.add(app)

    app.composer.value = "preserved draft"
    app.openPermissionPicker()
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "list_permissions",
      session_id: "session-permissions",
    }))
    const listPermissions = emitted.findLast((command) => command.type === "list_permissions")
    if (listPermissions?.type !== "list_permissions") {
      throw new Error("missing permission list command")
    }
    expect(app.picker.status.plainText).toContain("Loading permission rules")
    expect(app.picker.select.visible).toBeFalse()
    expect(app.picker.select.options).toHaveLength(0)
    setup.mockInput.pressEnter()
    await setup.mockInput.typeText("hidden input")
    await setup.mockInput.pasteBracketedText("hidden paste")
    expect(app.composer.value).toBe("preserved draft")
    expect(emitted.filter((command) => command.type === "list_permissions")).toHaveLength(1)
    app.handleEvent({
      type: "permissions_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "permission-driver",
        request_id: listPermissions.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-permissions",
      permissions: {
        default: "ask",
        effective_rules: [{ id: "effective:one", pattern: "bash(rm *)", action: "deny" }],
        project_rules: [],
        session_rules: [{ id: "session:one", pattern: "bash(cargo test*)", action: "ask" }],
        approvals: [{
          id: "session:opaque-approval",
          scope: "session",
          tool_name: "read",
          summary: "exact-invocation=hidden capabilities=ReadFilesystem approval=none",
        }],
        truncated: false,
      },
    })
    expect(app.picker.select.options.map((option) => option.value)).toContain(
      "permissions.effective.effective:one",
    )
    expect(app.picker.select.options.slice(0, 4).map((option) => option.value)).toEqual([
      "permissions.mode.strict",
      "permissions.mode.auto-safe",
      "permissions.mode.yolo",
      "permissions.mode.default",
    ])
    expect(app.picker.select.options[3]?.name).toBe("● default")
    expect(app.picker.status.visible).toBeFalse()
    expect(app.picker.select.visible).toBeTrue()
    const permissionCopy = app.picker.select.options
      .flatMap((option) => [option.name, option.description])
      .join("\n")
    expect(permissionCopy).not.toContain("Session-scoped")
    expect(permissionCopy).not.toContain("tool(argument")
    expect(permissionCopy).not.toContain("exact-invocation")

    const removeIndex = app.picker.select.options.findIndex(
      (option) => option.value === "permissions.remove.session:one",
    )
    app.picker.select.setSelectedIndex(removeIndex)
    app.picker.select.selectCurrent()
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "remove_session_permission_rule",
      rule_id: "session:one",
    }))

    const revokeIndex = app.picker.select.options.findIndex(
      (option) => option.value === "permissions.revoke.session:opaque-approval",
    )
    app.picker.select.setSelectedIndex(revokeIndex)
    app.picker.select.selectCurrent()
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "revoke_permission_approval",
      approval_id: "session:opaque-approval",
      scope: "session",
    }))
    expect(emitted.some((command) => command.type === "send_message")).toBe(false)
  })
})
