import type { ActivityTimingProjection, RottweilerState, ToolProjection } from "./model"

export const UNKNOWN_ACTIVITY_TIMING: ActivityTimingProjection = { kind: "unknown" }

export const MAX_RETAINED_TOOL_PROJECTIONS = 16

export function retainRecentTools(
  current: RottweilerState["tools"], invocationId: string, tool: ToolProjection,
): RottweilerState["tools"] {
  const next = { ...current, [invocationId]: tool }
  let count = Object.keys(next).length
  for (const [id, projection] of Object.entries(next)) {
    if (count <= MAX_RETAINED_TOOL_PROJECTIONS) break
    if (id !== invocationId && projection.status === "finished") {
      delete next[id]
      count -= 1
    }
  }
  return next
}

export function updateTool(
  current: RottweilerState["tools"],
  invocationId: string,
  tool: ToolProjection,
): RottweilerState["tools"] {
  return { ...current, [invocationId]: tool }
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
