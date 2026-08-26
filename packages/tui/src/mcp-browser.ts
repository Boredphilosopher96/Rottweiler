import { StyledText, bold, fg } from "@opentui/core"

import { fuzzyMatch, type FuzzyMatch } from "./components/picker"
import type {
  ListDetailItemRow,
  ListDetailPresentation,
} from "./components/list-detail"
import type { McpServerDescriptor, McpServerState } from "./protocol"
import type { RottweilerState } from "./state/model"
import type { RottweilerTheme } from "./theme"

export type McpBrowserAction =
  | { readonly kind: "manage"; readonly server: string }
  | { readonly kind: "addHttp" }
  | { readonly kind: "addStdio" }
  | { readonly kind: "retry" }

export type McpCatalog =
  | { readonly kind: "loading" }
  | { readonly kind: "ready"; readonly servers: RottweilerState["mcpServers"] }
  | {
      readonly kind: "error"
      readonly message: string
      readonly stale: RottweilerState["mcpServers"]
    }

export type McpStateTone = "muted" | "info" | "success" | "warning" | "error"

interface McpStatePresentation {
  readonly label: string
  readonly tone: McpStateTone
}

interface McpBrowserModelInput {
  readonly catalog: McpCatalog
  readonly review: RottweilerState["mcpApprovalReview"]
  readonly query: string
  readonly selectedId: string | null
}

export function createMcpBrowserModel(
  input: McpBrowserModelInput,
): ListDetailPresentation<McpBrowserAction> {
  if (input.catalog.kind === "loading") {
    return {
      title: "MCP   0 servers · 0 ready · 0 tools   /mcp",
      query: "",
      rows: [],
      selectedId: null,
      status: "Loading MCP connections",
      emptyCopy: "Loading MCP connections",
      notice: null,
    }
  }

  const servers = input.catalog.kind === "ready"
    ? input.catalog.servers
    : input.catalog.stale
  const query = input.query.trim()
  const candidates = [
    ...(input.catalog.kind === "error" ? [retryRow()] : []),
    addHttpRow(),
    addStdioRow(),
    ...servers.map((server) => serverRow(server, input.review)),
  ]
  const rows = candidates.filter((row) => matchesQuery(row, query))
  const selectedId = retainedSelection(rows, input.selectedId)
  const selected = rows.find((row) => row.id === selectedId)
  const ready = servers.filter((server) => server.state.type === "ready").length
  const tools = servers.reduce((total, server) => total + server.tool_count, 0)
  const notice = input.catalog.kind === "error"
    ? { message: input.catalog.message, tone: "error" as const }
    : null

  return {
    title: `MCP   ${servers.length} servers · ${ready} ready · ${tools} tools   /mcp`,
    query,
    rows,
    selectedId,
    status: `${actionHint(selected?.action)}${input.catalog.kind === "error" ? " · Ctrl-R retry" : ""} · Esc close`,
    emptyCopy: query.length === 0 ? "No MCP servers configured" : "No matching MCP connections",
    notice,
  }
}

export function mcpStatePresentation(state: McpServerState): McpStatePresentation {
  switch (state.type) {
    case "disabled": return { label: "Disabled", tone: "muted" }
    case "connecting": return { label: "Connecting", tone: "info" }
    case "ready": return { label: "Connected", tone: "success" }
    case "approval_required": return { label: "Approval needed", tone: "warning" }
    case "failed": return { label: "Connection failed", tone: "error" }
    case "stopping": return { label: "Stopping", tone: "warning" }
    default: return assertNever(state)
  }
}

export function mcpBrowserRow(
  row: ListDetailItemRow<McpBrowserAction>,
  server: McpServerDescriptor | undefined,
  selected: boolean,
  theme: RottweilerTheme,
): StyledText {
  const marker = fg(selected ? theme.primary : theme.textMuted)(selected ? "› " : "  ")
  if (row.action.kind !== "manage" || server === undefined) {
    const tone = row.action.kind === "retry" ? theme.error : theme.text
    return new StyledText([marker, selected ? bold(fg(tone)(row.label)) : fg(tone)(row.label)])
  }
  const state = mcpStatePresentation(server.state)
  const stateColor = toneColor(state.tone, theme)
  return new StyledText([
    marker,
    fg(theme.text)(server.name),
    fg(theme.textMuted)(" · "),
    selected ? bold(fg(stateColor)(state.label)) : fg(stateColor)(state.label),
    fg(theme.textMuted)(` · ${server.tool_count} tools`),
  ])
}

function serverRow(
  server: McpServerDescriptor,
  review: RottweilerState["mcpApprovalReview"],
): ListDetailItemRow<McpBrowserAction> {
  const state = mcpStatePresentation(server.state)
  const counts = `${server.tool_count} tools · ${server.resource_count} resources · ${server.prompt_count} prompts`
  const summary = server.state.type === "failed"
    ? `${state.label}: ${server.state.message} · ${counts}`
    : `${state.label} · ${server.enabled ? "enabled" : "disabled"} · ${server.approved ? "approved" : "approval needed"} · ${counts}`
  const matchingReview = review?.server === server.name ? review : null
  const description = [
    summary,
    "",
    `enabled     ${server.enabled ? "yes" : "no"}`,
    `approved    ${server.approved ? "yes" : "no"}`,
    `state       ${state.label}`,
    ...(server.state.type === "failed" ? [`message     ${server.state.message}`] : []),
    `tools       ${server.tool_count}`,
    `resources   ${server.resource_count}`,
    `prompts     ${server.prompt_count}`,
    ...(matchingReview === null
      ? []
      : [
          "",
          "Approval review",
          `transport   ${matchingReview.transport}`,
          `endpoint    ${matchingReview.endpoint ?? "local process"}`,
          `origin      ${matchingReview.origin}`,
          `defer tools ${matchingReview.defer_tools ? "yes" : "no"}`,
          `fingerprint ${matchingReview.fingerprint}`,
          `approved    ${matchingReview.previously_approved ? "previously" : "not previously"}`,
        ]),
  ].join("\n")
  return {
    kind: "item",
    id: `mcp.server.${server.name}`,
    label: `${server.name} · ${state.label} · ${server.tool_count} tools`,
    matchSpans: [],
    detail: { title: server.name, meta: state.label, description },
    action: { kind: "manage", server: server.name },
  }
}

function addHttpRow(): ListDetailItemRow<McpBrowserAction> {
  return {
    kind: "item",
    id: "mcp.add.http",
    label: "Add HTTPS server…",
    matchSpans: [],
    detail: {
      title: "Add HTTPS server",
      meta: "Remote HTTPS",
      description: "Register a remote HTTPS endpoint.\nNew servers start disabled.",
    },
    action: { kind: "addHttp" },
  }
}

function addStdioRow(): ListDetailItemRow<McpBrowserAction> {
  return {
    kind: "item",
    id: "mcp.add.stdio",
    label: "Add stdio server…",
    matchSpans: [],
    detail: {
      title: "Add stdio server",
      meta: "Local command",
      description: "Register a local executable and arguments.\nNew servers start disabled.",
    },
    action: { kind: "addStdio" },
  }
}

function retryRow(): ListDetailItemRow<McpBrowserAction> {
  return {
    kind: "item",
    id: "mcp.retry",
    label: "Retry inventory",
    matchSpans: [],
    detail: {
      title: "Retry MCP inventory",
      meta: "list_mcp_servers",
      description: "Request the current MCP server inventory again.",
    },
    action: { kind: "retry" },
  }
}

function matchesQuery(row: ListDetailItemRow<McpBrowserAction>, query: string): boolean {
  if (query.length === 0) return true
  return fuzzyMatch(query, `${row.label} ${row.detail.meta} ${row.detail.description}`) !== null
}

function retainedSelection(
  rows: readonly ListDetailItemRow<McpBrowserAction>[],
  requested: string | null,
): string | null {
  if (requested !== null && rows.some((row) => row.id === requested)) return requested
  return rows[0]?.id ?? null
}

function actionHint(action: McpBrowserAction | undefined): string {
  switch (action?.kind) {
    case "manage": return "Enter manage"
    case "addHttp":
    case "addStdio": return "Enter add"
    case "retry": return "Enter retry"
    default: return "No selection"
  }
}

function toneColor(tone: McpStateTone, theme: RottweilerTheme): string {
  switch (tone) {
    case "muted": return theme.textMuted
    case "info": return theme.info
    case "success": return theme.success
    case "warning": return theme.warning
    case "error": return theme.error
  }
}

function assertNever(value: never): never {
  throw new Error(`Unsupported MCP state: ${JSON.stringify(value)}`)
}
