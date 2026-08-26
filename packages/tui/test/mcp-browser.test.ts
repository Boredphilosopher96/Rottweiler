import { describe, expect, test } from "bun:test"

import {
  createMcpBrowserModel,
  mcpStatePresentation,
  type McpCatalog,
} from "../src/mcp-browser"
import type { McpServerDescriptor } from "../src/protocol"

const server = (value: McpServerDescriptor): McpServerDescriptor => value

const servers: readonly McpServerDescriptor[] = [
  server({ name: "docs.remote", enabled: true, approved: true, state: { type: "ready" }, tool_count: 6, resource_count: 2, prompt_count: 1 }),
  server({ name: "build.local", enabled: true, approved: true, state: { type: "connecting" }, tool_count: 3, resource_count: 0, prompt_count: 0 }),
  server({ name: "approval.pending", enabled: true, approved: false, state: { type: "approval_required" }, tool_count: 0, resource_count: 1, prompt_count: 0 }),
  server({ name: "broken.remote", enabled: true, approved: true, state: { type: "failed", message: "TLS certificate rejected" }, tool_count: 2, resource_count: 0, prompt_count: 0 }),
  server({ name: "disabled.local", enabled: false, approved: false, state: { type: "disabled" }, tool_count: 1, resource_count: 0, prompt_count: 0 }),
  server({ name: "stopping.local", enabled: false, approved: true, state: { type: "stopping" }, tool_count: 0, resource_count: 0, prompt_count: 0 }),
]

describe("MCP browser model", () => {
  test("derives truthful rows, counts, state tones, failure copy, and matching review detail", () => {
    const model = createMcpBrowserModel({
      catalog: { kind: "ready", servers },
      review: {
        server: "docs.remote",
        transport: "streamable_http",
        endpoint: "https://docs.example/mcp",
        origin: "user configuration",
        defer_tools: true,
        fingerprint: "sha256:docs",
        previously_approved: true,
      },
      query: "",
      selectedId: "mcp.server.docs.remote",
    })

    expect(model.title).toBe("MCP   6 servers · 1 ready · 12 tools   /mcp")
    expect(model.rows.map((row) => row.id)).toEqual([
      "mcp.add.http",
      "mcp.add.stdio",
      "mcp.server.docs.remote",
      "mcp.server.build.local",
      "mcp.server.approval.pending",
      "mcp.server.broken.remote",
      "mcp.server.disabled.local",
      "mcp.server.stopping.local",
    ])
    expect(model.rows.find((row) => row.id === "mcp.server.broken.remote")).toMatchObject({
      kind: "item",
      label: "broken.remote · Connection failed · 2 tools",
      detail: { description: expect.stringContaining("TLS certificate rejected") },
    })
    expect(model.rows.find((row) => row.id === "mcp.server.docs.remote")).toMatchObject({
      detail: {
        description: expect.stringContaining("transport   streamable_http"),
      },
    })
    const buildRow = model.rows.find((row) => row.id === "mcp.server.build.local")
    if (buildRow?.kind !== "item") throw new Error("missing build.local row")
    expect(buildRow.detail.description).not.toContain("sha256:docs")
    expect([
      mcpStatePresentation({ type: "disabled" }),
      mcpStatePresentation({ type: "connecting" }),
      mcpStatePresentation({ type: "ready" }),
      mcpStatePresentation({ type: "approval_required" }),
      mcpStatePresentation({ type: "failed", message: "failed" }),
      mcpStatePresentation({ type: "stopping" }),
    ]).toEqual([
      { label: "Disabled", tone: "muted" },
      { label: "Connecting", tone: "info" },
      { label: "Connected", tone: "success" },
      { label: "Approval needed", tone: "warning" },
      { label: "Connection failed", tone: "error" },
      { label: "Stopping", tone: "warning" },
    ])
    expect(JSON.stringify(model)).not.toMatch(/context tokens|eager cost|capability|allowlist|reauthorize|sandbox|TOON|rw serve --mcp/i)
  })

  test("filters across failure copy, retains selection, and distinguishes loading, empty, and stale failure", () => {
    const filtered = createMcpBrowserModel({
      catalog: { kind: "ready", servers },
      review: null,
      query: "certificate",
      selectedId: "mcp.server.docs.remote",
    })
    expect(filtered.rows.map((row) => row.id)).toEqual(["mcp.server.broken.remote"])
    expect(filtered.selectedId).toBe("mcp.server.broken.remote")

    expect(createMcpBrowserModel({ catalog: { kind: "loading" }, review: null, query: "", selectedId: null })).toMatchObject({
      rows: [],
      selectedId: null,
      emptyCopy: "Loading MCP connections",
    })

    const empty = createMcpBrowserModel({ catalog: { kind: "ready", servers: [] }, review: null, query: "", selectedId: null })
    expect(empty.rows.map((row) => row.id)).toEqual(["mcp.add.http", "mcp.add.stdio"])

    const failed: McpCatalog = { kind: "error", message: "MCP discovery timed out", stale: servers.slice(0, 1) }
    const stale = createMcpBrowserModel({ catalog: failed, review: null, query: "", selectedId: null })
    expect(stale.rows.map((row) => row.id)).toEqual([
      "mcp.retry",
      "mcp.add.http",
      "mcp.add.stdio",
      "mcp.server.docs.remote",
    ])
    expect(stale.notice).toEqual({ message: "MCP discovery timed out", tone: "error" })
  })
})
