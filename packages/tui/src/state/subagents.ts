import { MAX_CHILD_TASK_PREVIEW_BYTES } from "../../../../protocol/types"
import {
  type EngineEvent
} from "../protocol"
import { isRecord } from "../transport"
import { boundedUtf8 } from "./display-buffer"
import {
  type RottweilerState
} from "./model"

export const MAX_SUBAGENT_TASK_BYTES = MAX_CHILD_TASK_PREVIEW_BYTES

export const MAX_TERMINAL_SUBAGENT_HISTORY = 128

export function nextSubagentArchiveKey(
  subagents: RottweilerState["subagents"],
  subagentId: string,
  parentTurnId: string,
): string {
  const base = `${subagentId}@${parentTurnId}`
  if (subagents[base] === undefined) return base
  let ordinal = 2
  while (subagents[`${base}#${ordinal}`] !== undefined) ordinal += 1
  return `${base}#${ordinal}`
}

export function boundedSubagentHistory(
  subagents: RottweilerState["subagents"],
  order: readonly string[],
): Pick<RottweilerState, "subagents" | "subagentOrder"> {
  const terminalIds = order.filter((id) => subagents[id]?.status !== "running")
  const retainedTerminalIds = new Set(terminalIds.slice(-MAX_TERMINAL_SUBAGENT_HISTORY))
  const subagentOrder = order.filter((id) => {
    const projection = subagents[id]
    return (
      projection !== undefined &&
      (projection.status === "running" || retainedTerminalIds.has(id))
    )
  })
  const retainedSubagents = Object.fromEntries(
    subagentOrder.map((id) => [id, subagents[id]!] as const),
  )
  return { subagents: retainedSubagents, subagentOrder }
}

const TERMINAL_SUBAGENT_STATUSES = new Set([
  "completed",
  "failed",
  "cancelled",
  "timed_out",
  "max_turns",
] as const)

export function subagentTerminalSummary(
  result: Extract<EngineEvent, { type: "subagent_finished" }>["result"],
): {
  readonly status: "completed" | "failed" | "cancelled" | "timed_out" | "max_turns"
  readonly childSessionId: string | null
  readonly summary: string | null
  readonly touchedFileCount: number
  readonly diffArtifactId: string | null
} {
  return {
    status: TERMINAL_SUBAGENT_STATUSES.has(result.status)
      ? result.status
      : "failed",
    childSessionId: result.session_id,
    summary: boundedSummary(result.final_text),
    touchedFileCount: result.touched_files.length,
    diffArtifactId: result.diff_artifact?.id ?? null,
  }
}

function boundedSummary(value: string): string {
  return boundedUtf8(value, 512)
}

export function subagentActivity(event: unknown): string {
  if (!isRecord(event) || typeof event.type !== "string") {
    return "working"
  }
  switch (event.type) {
    case "turn_started":
      return "working"
    case "thinking_delta":
      return "thinking"
    case "text_delta":
      return "writing response"
    case "tool_call_started": {
      if (typeof event.name !== "string") return "using tool"
      const toolName = compactActivityValue(event.name, 24) ?? "tool"
      const detail = safeSubagentToolDetail(event.name, event.args)
      return boundedActivity(`using tool · ${toolName}${detail === null ? "" : ` · ${detail}`}`)
    }
    case "tool_approval_needed":
      return typeof event.name === "string"
        ? `awaiting approval · ${event.name}`
        : "awaiting approval"
    case "tool_diff_ready":
      return "prepared diff"
    case "tool_output_delta":
      return "receiving tool output"
    case "tool_call_finished":
      return "tool finished"
    case "question_asked":
      return "awaiting answer"
    case "turn_finished":
      return "finalizing"
    case "error":
      return "error"
    default:
      return event.type.replaceAll("_", " ")
  }
}

function safeSubagentToolDetail(name: string, args: unknown): string | null {
  try {
    return subagentToolDetail(name, args)
  } catch {
    return null
  }
}

function subagentToolDetail(name: string, args: unknown): string | null {
  if (!isRecord(args)) return null
  const normalized = name.toLowerCase()
  if (normalized === "bash" || normalized === "shell") {
    return compactActivityValue(firstString(args, ["command", "cmd"]), 48)
  }
  if (normalized === "read" || normalized === "write" || normalized === "edit") {
    const path = firstString(args, ["path", "file_path", "filePath"])
    return path === null ? null : compactSubagentPath(path, 48)
  }
  if (normalized === "grep" || normalized === "glob") {
    return compactActivityValue(firstString(args, ["pattern", "query", "regex"]), 48)
  }
  return null
}

function firstString(
  record: Readonly<Record<string, unknown>>,
  keys: readonly string[],
): string | null {
  for (const key of keys) {
    const value = record[key]
    if (typeof value === "string") return value
  }
  return null
}

function compactSubagentPath(value: string, limit: number): string | null {
  const compact = value.replaceAll("\\", "/").replace(/\s+/g, " ").trim()
  if (compact === "") return null
  const parts = compact.split("/").filter(Boolean)
  const tail = parts.length <= 2 ? parts.join("/") : parts.slice(-2).join("/")
  if (tail.length <= limit) return tail
  return `…${tail.slice(-(limit - 1))}`
}

function compactActivityValue(value: string | null, limit: number): string | null {
  if (value === null) return null
  const compact = value.replace(/\s+/g, " ").trim()
  if (compact === "") return null
  return compact.length <= limit ? compact : `${compact.slice(0, limit - 1)}…`
}

function boundedActivity(value: string): string {
  const compact = value.replace(/\s+/g, " ").trim()
  return compact.length <= 72 ? compact : `${compact.slice(0, 71)}…`
}
