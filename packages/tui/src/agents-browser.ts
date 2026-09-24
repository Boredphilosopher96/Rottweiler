import { fuzzyMatch } from "./components/picker"
import type {
  ListDetailItemRow,
  ListDetailPresentation,
  ListDetailRow,
} from "./components/list-detail"
import { QUEUED_GLYPH, formatSubagentElapsed } from "./components/agents-strip"
import { subagentGlyph } from "./components/transcript/blocks"
import type { FamilyControlRow } from "../../../protocol/types"
import { formatKnownCost, modelDisplayLabel } from "./render"
import type { RottweilerState, SubagentProjection } from "./state"
import type { SubagentDescriptor } from "./subagent-state"
import { boundedUiText } from "./ui-presentation"

/** What Enter does on one Agents screen row. */
export type AgentsBrowserAction =
  /** Open the per-child actions: view, message, stop, background, close. */
  | { readonly kind: "child"; readonly subagentId: string }
  /** Open a child whose approval, question, or plan waits on the user. */
  | { readonly kind: "control"; readonly row: FamilyControlRow }
  | { readonly kind: "retry" }

/** Footer chord that clears finished agents from the strip above the composer. */
export const HIDE_FINISHED_KEY = "ctrl+d"

/** One child known from the live projection, the engine catalog, or both. */
export interface AgentsBrowserChild {
  readonly subagentId: string
  readonly projection: SubagentProjection | null
  readonly descriptor: SubagentDescriptor | null
}

interface AgentsBrowserModelInput {
  readonly state: RottweilerState
  readonly catalog: readonly SubagentDescriptor[]
  readonly pending: readonly FamilyControlRow[]
  readonly foreground: string | null
  readonly finishedInStrip: number
  readonly loading: boolean
  readonly error: string | null
  readonly query: string
  readonly selectedId: string | null
  readonly nowMs?: number
}

const TITLE = "AGENTS   /agents"

/** Every child of the session: live projections first, then catalog-only children. */
export function agentsBrowserChildren(
  state: RottweilerState,
  catalog: readonly SubagentDescriptor[],
): readonly AgentsBrowserChild[] {
  const children: AgentsBrowserChild[] = []
  for (const key of state.subagentOrder) {
    const projection = state.subagents[key]
    if (projection === undefined || projection.projectionId !== projection.subagentId) continue
    children.push({
      subagentId: projection.subagentId,
      projection,
      descriptor: catalog.find((descriptor) => descriptor.subagent_id === projection.subagentId) ?? null,
    })
  }
  for (const descriptor of catalog) {
    if (children.some((child) => child.subagentId === descriptor.subagent_id)) continue
    children.push({ subagentId: descriptor.subagent_id, projection: null, descriptor })
  }
  return children
}

/** Admitted but still waiting for a free child slot. */
export function childQueued(child: AgentsBrowserChild): boolean {
  return child.descriptor?.activity === "queued" && (child.projection === null || child.projection.status === "running")
}

export function childStatus(child: AgentsBrowserChild): string {
  if (childQueued(child)) return "queued"
  return child.projection?.status.replaceAll("_", " ") ?? child.descriptor?.activity ?? "unknown"
}

/** Live children: running or queued. */
export function childRunning(child: AgentsBrowserChild): boolean {
  if (childQueued(child)) return true
  return child.projection === null ? child.descriptor?.activity === "running" : child.projection.status === "running"
}

export function childAgentName(child: AgentsBrowserChild): string {
  return child.descriptor?.agent || "agent"
}

export function childTask(child: AgentsBrowserChild): string {
  return child.projection?.task ?? child.descriptor?.task ?? child.subagentId
}

/** List-detail projection of every child agent in the session. */
export function createAgentsBrowserModel(
  input: AgentsBrowserModelInput,
): ListDetailPresentation<AgentsBrowserAction> {
  const query = input.query.trim()
  const children = agentsBrowserChildren(input.state, input.catalog)
  if (input.error !== null && children.length === 0 && input.pending.length === 0) {
    return {
      title: TITLE,
      query,
      rows: [{
        kind: "item", id: "agents.retry", label: "Retry loading agents", matchSpans: [],
        detail: { title: "Agents could not be loaded", meta: "", description: input.error },
        action: { kind: "retry" },
      }],
      selectedId: "agents.retry",
      status: "Enter retry · Esc close",
      notice: { message: input.error, tone: "error" },
    }
  }
  const rows: ListDetailRow<AgentsBrowserAction>[] = []
  const pendingItems = input.pending.map((row): ListDetailItemRow<AgentsBrowserAction> => {
    const child = children.find((candidate) => candidate.projection?.childSessionId === row.target.session_id ||
      candidate.descriptor?.child_session_id === row.target.session_id)
    const name = child === undefined ? "agent" : childAgentName(child)
    const needs = [
      ...(row.controls.approvals > 0 ? [`${row.controls.approvals} approval${row.controls.approvals === 1 ? "" : "s"}`] : []),
      ...(row.controls.questions > 0 ? [`${row.controls.questions} question${row.controls.questions === 1 ? "" : "s"}`] : []),
      ...(row.controls.pending_plan ? ["plan review"] : []),
    ].join(" · ")
    return {
      kind: "item",
      id: `agents.control.${row.target.session_id}`,
      label: `! ${name} · ${boundedUiText(child === undefined ? row.target.session_id : childTask(child), 96)}`,
      matchSpans: [],
      detail: {
        title: `${name} needs a response`,
        meta: needs,
        description: `${needs}\n\nEnter opens this agent so you can respond. The parent keeps running.`,
      },
      action: { kind: "control", row },
    }
  })
  pushSection(rows, "agents.section.pending", "Needs response", filterRows(pendingItems, query))
  // A child waiting on the user is listed once, under "Needs response".
  const listed = children.filter((child) => !input.pending.some((row) =>
    row.target.session_id === (child.projection?.childSessionId ?? child.descriptor?.child_session_id)))
  const running = listed.filter(childRunning)
  const finished = listed.filter((child) => !childRunning(child))
  pushSection(rows, "agents.section.running", "Running", filterRows(running.map((child) => childRow(child, input)), query))
  const finishedRows = finished.map((child) => childRow(child, input))
  pushSection(rows, "agents.section.finished", "Finished", filterRows(finishedRows, query))
  const items = rows.filter((row): row is ListDetailItemRow<AgentsBrowserAction> => row.kind === "item")
  const selected = items.find((row) => row.id === input.selectedId) ?? items[0]
  const queuedCount = running.filter(childQueued).length
  const count = `${running.length - queuedCount} running${queuedCount === 0 ? "" : ` · ${queuedCount} queued`} · ${finished.length} finished`
  return {
    title: TITLE,
    query,
    rows,
    selectedId: selected?.id ?? null,
    status: [
      ...(selected === undefined ? [] : [selectedHint(selected.action)]),
      ...(input.finishedInStrip > 0 ? ["Ctrl+D clear finished from strip"] : []),
      count,
      "Esc close",
    ].join(" · "),
    emptyCopy: input.loading ? "Loading agents" : query.length > 0 ? "No matching agents" : "No child agents yet. Agents started by this session appear here.",
    notice: input.error === null ? null : { message: input.error, tone: "warning" },
  }
}

function childRow(child: AgentsBrowserChild, input: AgentsBrowserModelInput): ListDetailItemRow<AgentsBrowserAction> {
  const projection = child.projection
  const name = childAgentName(child)
  const status = childStatus(child)
  const queued = childQueued(child)
  const elapsed = projection?.status === "running" && !queued ? formatSubagentElapsed(projection.spawnedAtMs, input.nowMs) : null
  const foreground = input.foreground === child.subagentId
  const glyph = queued ? QUEUED_GLYPH : projection === null ? (childRunning(child) ? "◌" : "·") : subagentGlyph(projection.status)
  const summary = projection?.summary ?? null
  const cost = formatKnownCost(projection?.cost)
  const model = child.descriptor === null ? null
    : modelDisplayLabel(child.descriptor.model, input.state.models) ?? child.descriptor.model
  return {
    kind: "item",
    id: `agents.child.${child.subagentId}`,
    label: [
      `${glyph} ${name} · ${boundedUiText(childTask(child), 96)}`,
      ...(elapsed === null ? [] : [elapsed]),
      ...(cost === null ? [] : [cost]),
    ].join(" · "),
    matchSpans: [],
    detail: {
      title: `${name} · ${status}`,
      meta: [
        ...(child.descriptor === null || model === null ? [] : [model, child.descriptor.isolation]),
        ...(foreground ? ["parent is waiting"] : []),
      ].join(" · "),
      // The first line doubles as the compact detail on narrow screens.
      description: [
        boundedUiText(summary ?? childTask(child), 512),
        "",
        `status     ${status}${queued ? " · waiting for a free agent slot" : projection?.status === "running" && projection.activity !== null ? ` · ${projection.activity}` : ""}`,
        ...(elapsed === null ? [] : [`elapsed    ${elapsed}`]),
        ...(cost === null ? [] : [`cost       ${cost}`]),
        ...(projection === null || projection.status === "running" ? [] : [`changed    ${projection.touchedFileCount} file${projection.touchedFileCount === 1 ? "" : "s"}`]),
        ...(summary === null ? [] : [`task       ${boundedUiText(childTask(child), 512)}`]),
        "",
        "Enter view · message · stop · close",
      ].join("\n"),
    },
    action: { kind: "child", subagentId: child.subagentId },
  }
}

function selectedHint(action: AgentsBrowserAction): string {
  switch (action.kind) {
    case "child": return "Enter actions"
    case "control": return "Enter respond"
    case "retry": return "Enter retry"
  }
}

function filterRows(
  rows: readonly ListDetailItemRow<AgentsBrowserAction>[],
  query: string,
): readonly ListDetailItemRow<AgentsBrowserAction>[] {
  if (query.length === 0) return rows
  return rows.filter((row) => fuzzyMatch(query, `${row.label} ${row.detail.meta} ${row.detail.title}`) !== null)
}

function pushSection(
  rows: ListDetailRow<AgentsBrowserAction>[],
  id: string,
  label: string,
  items: readonly ListDetailItemRow<AgentsBrowserAction>[],
): void {
  if (items.length === 0) return
  rows.push({ kind: "section", id, label }, ...items)
}
