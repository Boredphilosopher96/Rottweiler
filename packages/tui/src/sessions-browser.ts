import { fuzzyMatch } from "./components/picker"
import type { ListDetailItemRow, ListDetailPresentation } from "./components/list-detail"
import type { RottweilerState } from "./state/model"

type Session = RottweilerState["sessions"][number]

export type SessionsBrowserAction =
  | { readonly kind: "new" }
  | { readonly kind: "manage"; readonly session: Session }
  | { readonly kind: "retry" }

export type SessionsCatalog =
  | { readonly kind: "loading" }
  | { readonly kind: "ready"; readonly sessions: RottweilerState["sessions"]; readonly truncated: boolean }
  | { readonly kind: "error"; readonly message: string; readonly stale: RottweilerState["sessions"] }

export function createSessionsBrowserModel(input: {
  readonly catalog: SessionsCatalog
  readonly query: string
  readonly selectedId: string | null
}): ListDetailPresentation<SessionsBrowserAction> {
  if (input.catalog.kind === "loading") return empty("Loading sessions", "Loading sessions")
  const sessions = input.catalog.kind === "ready" ? input.catalog.sessions : input.catalog.stale
  const query = input.query.trim()
  const sessionRows = sessions
    .filter((session) => query.length === 0 || fuzzyMatch(query, sessionText(session)) !== null)
    .map(sessionRow)
  const rows: readonly ListDetailItemRow<SessionsBrowserAction>[] = [
    {
      kind: "item", id: "sessions.new", label: "New session", matchSpans: [],
      detail: { title: "New session", meta: "current workspace", description: "Start a clean conversation in this workspace." },
      action: { kind: "new" },
    },
    ...(input.catalog.kind === "error" ? [{
      kind: "item" as const, id: "sessions.retry", label: "Retry loading sessions", matchSpans: [],
      detail: { title: "Sessions unavailable", meta: "request failed", description: input.catalog.message },
      action: { kind: "retry" as const },
    }] : []),
    ...sessionRows,
  ]
  const selectedId = rows.some((row) => row.id === input.selectedId) ? input.selectedId : rows[0]?.id ?? null
  return {
    title: input.catalog.kind === "ready" && input.catalog.truncated ? "SESSIONS   results truncated" : "SESSIONS   /sessions",
    query,
    rows,
    selectedId,
    status: "Enter open · Ctrl-N new · Esc close",
    emptyCopy: "No matching sessions",
    notice: input.catalog.kind === "error" ? { message: input.catalog.message, tone: "error" } : null,
  }
}

function sessionRow(session: Session): ListDetailItemRow<SessionsBrowserAction> {
  const title = session.title || session.workspaceName
  return {
    kind: "item",
    id: session.sessionId,
    label: title,
    matchSpans: [],
    detail: {
      title,
      meta: session.sessionId,
      description: [
        `workspace   ${session.workspaceName}`,
        `model       ${session.model}`,
        `shell       ${session.shellActive ? "active" : "idle"}`,
        "",
        "Enter session actions",
      ].join("\n"),
    },
    action: { kind: "manage", session },
  }
}

function sessionText(session: Session): string {
  return `${session.sessionId} ${session.title ?? ""} ${session.workspaceName} ${session.model}`
}

function empty(emptyCopy: string, status: string): ListDetailPresentation<SessionsBrowserAction> {
  return { title: "SESSIONS   /sessions", query: "", rows: [], selectedId: null, status, emptyCopy, notice: null }
}
