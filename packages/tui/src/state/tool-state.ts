import {
  type ToolOutput
} from "../protocol"
import { isRecord, parseU64 } from "../transport"
import {
  type ActivityTimingProjection,
  type RottweilerState,
  type TodoProjection,
  type ToolProjection
} from "./model"
import { MAX_RETAINED_TRANSCRIPT_ENTRIES } from "./turn-state"

export const UNKNOWN_ACTIVITY_TIMING: ActivityTimingProjection = { kind: "unknown" }

export const MAX_TODO_ITEMS = 128

export const MAX_TODO_ID_BYTES = 256

export const MAX_TODO_CONTENT_BYTES = 4_096

export const MAX_TODO_TOTAL_BYTES = 64 * 1_024

export const MAX_RETAINED_TOOL_PROJECTIONS = 16

export const MAX_RETAINED_TODO_TOOL_PROJECTIONS = MAX_RETAINED_TRANSCRIPT_ENTRIES

export function retainRecentTools(
  current: RottweilerState["tools"],
  toolCallId: string,
  tool: ToolProjection,
): RottweilerState["tools"] {
  const next = { ...current, [toolCallId]: tool }

  // A successful todo call is also a rewind checkpoint. Keep the latest
  // checkpoint for every retained turn independently from the small display
  // cache, otherwise ordinary tool traffic can make rewind restore stale todos.
  const todoCheckpoints = Object.entries(next).filter(([, projection]) =>
    isSuccessfulTodoCheckpoint(projection)
  )
  const latestTodoByTurn = new Map<string, readonly [string, ToolProjection]>()
  for (const entry of todoCheckpoints) {
    const existing = latestTodoByTurn.get(entry[1].turnId)
    if (existing === undefined || compareToolOrder(existing[1], entry[1]) < 0) {
      latestTodoByTurn.set(entry[1].turnId, entry)
    }
  }
  const retainedTodoIds = new Set([...latestTodoByTurn.values()].map(([id]) => id))
  for (const [id] of todoCheckpoints) {
    if (!retainedTodoIds.has(id)) delete next[id]
  }

  const orderedTodos = [...latestTodoByTurn.values()].sort((left, right) =>
    compareToolOrder(left[1], right[1])
  )
  for (const [id] of orderedTodos.slice(0, -MAX_RETAINED_TODO_TOOL_PROJECTIONS)) {
    delete next[id]
    retainedTodoIds.delete(id)
  }

  const entries = Object.entries(next)
  let regularCount = entries.filter(([id]) => !retainedTodoIds.has(id)).length
  if (regularCount <= MAX_RETAINED_TOOL_PROJECTIONS) return next
  const removable = entries.filter(
    ([id, projection]) => !retainedTodoIds.has(id) && projection.status === "finished",
  )
  for (const [id] of removable) {
    if (regularCount <= MAX_RETAINED_TOOL_PROJECTIONS) break
    if (id !== toolCallId) delete next[id]
    if (id !== toolCallId) regularCount -= 1
  }
  return next
}

export function updateTool(
  current: RottweilerState["tools"],
  toolCallId: string,
  tool: ToolProjection,
): RottweilerState["tools"] {
  return { ...current, [toolCallId]: tool }
}

function isSuccessfulTodoCheckpoint(tool: ToolProjection): boolean {
  return (
    tool.name === "todo" &&
    tool.status === "finished" &&
    tool.isError === false &&
    tool.output !== null &&
    projectTodoOutput(tool.output) !== null
  )
}

function compareToolOrder(left: ToolProjection, right: ToolProjection): number {
  const leftTurn = parseU64(left.turnId)
  const rightTurn = parseU64(right.turnId)
  if (leftTurn !== null && rightTurn !== null) {
    if (leftTurn < rightTurn) return -1
    if (leftTurn > rightTurn) return 1
  } else {
    const turnOrder = left.turnId.localeCompare(right.turnId)
    if (turnOrder !== 0) return turnOrder
  }
  if (left.callIndex !== right.callIndex) return left.callIndex - right.callIndex
  return left.toolCallId.localeCompare(right.toolCallId)
}

function activityTimestamp(value: string): number | null {
  const timestamp = Date.parse(value)
  return Number.isFinite(timestamp) ? timestamp : null
}

export function openActivityTiming(emittedAt: string): ActivityTimingProjection {
  const timestamp = activityTimestamp(emittedAt)
  return timestamp === null
    ? UNKNOWN_ACTIVITY_TIMING
    : { kind: "open", startedAtMs: timestamp, lastObservedAtMs: timestamp }
}

export function observeActivityTiming(
  current: ActivityTimingProjection | undefined,
  emittedAt: string,
): ActivityTimingProjection {
  if (current?.kind !== "open") return current ?? UNKNOWN_ACTIVITY_TIMING
  const timestamp = activityTimestamp(emittedAt)
  return timestamp === null
    ? current
    : {
      kind: "open",
      startedAtMs: current.startedAtMs,
      lastObservedAtMs: Math.max(current.lastObservedAtMs, timestamp),
    }
}

export function closeActivityTiming(
  current: ActivityTimingProjection | undefined,
  emittedAt: string,
): ActivityTimingProjection {
  const timestamp = activityTimestamp(emittedAt)
  if (timestamp === null) return UNKNOWN_ACTIVITY_TIMING
  return {
    kind: "closed",
    startedAtMs: current?.kind === "open" ? current.startedAtMs : null,
    finishedAtMs: timestamp,
  }
}

export function deriveTodosFromTools(
  tools: RottweilerState["tools"],
  throughTurn: bigint,
): readonly TodoProjection[] {
  const candidates = Object.values(tools)
    .flatMap((tool) => {
      const turn = parseU64(tool.turnId)
      return tool.name === "todo" && tool.status === "finished" && tool.isError === false && turn !== null && turn <= throughTurn
        ? [{ tool, turn }]
        : []
    })
    .sort((left, right) => {
      if (left.turn < right.turn) return -1
      if (left.turn > right.turn) return 1
      if (left.tool.callIndex !== right.tool.callIndex) {
        return left.tool.callIndex - right.tool.callIndex
      }
      return left.tool.toolCallId.localeCompare(right.tool.toolCallId)
    })

  let todos: readonly TodoProjection[] = []
  for (const { tool } of candidates) {
    if (tool.output === null) continue
    const projected = projectTodoOutput(tool.output)
    if (projected !== null) todos = projected
  }
  return todos
}

/** Accept only the exact bounded structured snapshot emitted by TodoTool. */
export function projectTodoOutput(output: ToolOutput): readonly TodoProjection[] | null {
  const values =
    output.type === "structured"
      ? [output.value]
      : output.type === "mixed"
        ? output.parts.flatMap((part) => (part.type === "structured" ? [part.value] : []))
        : []
  for (let index = values.length - 1; index >= 0; index -= 1) {
    const projected = projectTodoValue(values[index])
    if (projected !== null) return projected
  }
  return null
}

function projectTodoValue(value: unknown): readonly TodoProjection[] | null {
  if (!isRecord(value)) return null
  let payload: Record<string, unknown> = value
  if ("data" in value || "truncated" in value) {
    if (value.truncated !== false || !isRecord(value.data)) return null
    payload = value.data
  }
  if (!Array.isArray(payload.items) || payload.items.length > MAX_TODO_ITEMS) return null
  if (
    typeof payload.count !== "number" ||
    !Number.isSafeInteger(payload.count) ||
    payload.count !== payload.items.length
  ) {
    return null
  }

  const ids = new Set<string>()
  const projected: TodoProjection[] = []
  let totalBytes = 0
  for (const item of payload.items) {
    if (
      !isRecord(item) ||
      typeof item.id !== "string" ||
      typeof item.content !== "string" ||
      !isTodoStatus(item.status) ||
      item.id.length === 0 ||
      item.content.length === 0 ||
      ids.has(item.id)
    ) {
      return null
    }
    const idBytes = Buffer.byteLength(item.id)
    const contentBytes = Buffer.byteLength(item.content)
    totalBytes += idBytes + contentBytes
    if (
      idBytes > MAX_TODO_ID_BYTES ||
      contentBytes > MAX_TODO_CONTENT_BYTES ||
      totalBytes > MAX_TODO_TOTAL_BYTES
    ) {
      return null
    }
    ids.add(item.id)
    projected.push({ id: item.id, content: item.content, status: item.status })
  }
  return projected
}

function isTodoStatus(value: unknown): value is TodoProjection["status"] {
  return value === "pending" || value === "in_progress" || value === "completed" || value === "blocked"
}
