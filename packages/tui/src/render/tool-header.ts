import { fg, t } from "@opentui/core"
import type { RottweilerTheme } from "../theme"
import { formatToolSubject } from "../tool-arguments"
import { stringCellWidth as cellWidth, truncateToCells } from "./text"
import { displayPath } from "./tool-presentation"
import { toolDurationMs, type ToolRow } from "./tool-row"

/** One-line humanized description: `Read calc.py`, `Search "foo" in src/`. */
export interface ToolSummary {
  readonly verb: string
  readonly subject: string
  /** Secondary facts such as `+1 −1` or `12 matches`; empty when none apply. */
  readonly detail: string
}

const SPINNER = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"] as const
export const TOOL_SPINNER_INTERVAL_MS = 120

export function toolSummary(row: ToolRow): ToolSummary {
  const args = row.args
  const text = (key: string): string => {
    const value = args?.[key]
    return typeof value === "string" ? oneLine(value) : ""
  }
  const path = (key = "path"): string => {
    const value = text(key)
    return value === "" ? "" : displayPath(value)
  }
  const where = (key = "path"): string => {
    const value = path(key)
    return value === "" || value === "." ? "" : ` in ${value}`
  }
  const failure = row.status === "finished" && row.isError ? failureDetail(row) : ""
  const stats = row.diffStats === null ? "" : `+${row.diffStats.added} −${row.diffStats.removed}`
  const counted = (singular: string, plural: string): string => {
    const count = resultCount(row)
    return count === null ? "" : `${count} ${count === 1 ? singular : plural}`
  }
  const pick = (...values: string[]): string => values.find(value => value !== "") ?? ""
  const summary = (verb: string, subject: string, detail = ""): ToolSummary =>
    ({ verb, subject, detail: failure !== "" ? failure : detail })
  if (args === null && row.argumentsNote !== null) return summary(builtinVerb(row.name), row.argumentsNote)
  switch (row.name) {
    case "read": {
      const start = typeof args?.start_line === "number" ? args.start_line : null
      const count = typeof args?.line_count === "number" ? args.line_count : null
      const range = start === null && count === null ? ""
        : count === null ? `from line ${start}` : `lines ${start ?? 1}–${(start ?? 1) + count - 1}`
      return summary("Read", path(), range)
    }
    case "write": return summary("Write", path(), stats)
    case "edit": return summary("Edit", path(), stats)
    case "multi_edit": {
      const edits = Array.isArray(args?.edits) ? args.edits.length : null
      return summary("Edit", path(), [stats, edits === null ? "" : `${edits} ${edits === 1 ? "edit" : "edits"}`].filter(Boolean).join(" · "))
    }
    case "bash": case "shell": {
      const command = text("command")
      return summary("Bash", `${firstLine(command)}${command.includes("\n") ? " …" : ""}`)
    }
    case "grep": case "search": return summary("Search", `${quoted(text("pattern"))}${where()}`, counted("match", "matches"))
    case "glob": return summary("Find", `${quoted(text("pattern"))}${where()}`, counted("file", "files"))
    case "ls": return summary("List", pick(path(), "."), counted("entry", "entries"))
    case "webfetch": return summary("Fetch", text("url"))
    case "websearch": return summary("Search web", quoted(text("query")), counted("result", "results"))
    case "todo": return summary("Update todos", todoSubject(args))
    case "ask_user": return summary("Ask", quoted(text("question")))
    case "submit_plan": return summary("Submit plan", "")
    case "skill": return summary("Skill", pick(text("name")), path())
    case "spawn_agent": case "agent": return agentSummary(args, summary)
    case "symbols": return summary("Find symbols", quoted(text("pattern")))
    case "diagnostics": return summary("Diagnostics", path())
    case "definition": return summary("Definition", position(path(), args))
    case "references": return summary("References", position(path(), args))
    case "rename": return summary("Rename", `${position(path(), args)}${text("new_name") === "" ? "" : ` → ${text("new_name")}`}`)
    case "background_status": return summary("Background status", pick(text("process_id"), "all processes"))
    case "background_output": return summary("Background output", text("process_id"))
    case "background_kill": return summary("Stop background", text("process_id"))
    case "apply_worktree_diff": return summary("Apply changes", pick(text("artifact_id"), "child diff"))
    case "tool_search": return summary("Find tools", `${quoted(text("query"))}${text("server") === "" ? "" : ` in ${text("server")}`}`)
    case "mcp_call": return summary(`${pick(text("server"), "MCP")} · ${pick(text("name"), "tool")}`, firstString(args?.arguments))
    case "mcp_overflow_read": return summary("Read MCP result", quoted(text("query")))
  }
  const mcp = /^mcp__(.+?)__(.+)$/.exec(row.name)
  if (mcp !== null) return summary(`${mcp[1]} · ${mcp[2]}`, firstString(args))
  return summary(row.name, firstString(args))
}

/** Collapsed or expanded, the header is one line: status bullet, summary, and outcome. */
export function toolHeaderContent(row: ToolRow, availableWidth: number, theme: RottweilerTheme, nowMs = Date.now(), spinnerFrame = 0): ReturnType<typeof t> {
  const summary = toolSummary(row)
  const duration = toolDurationMs(row, nowMs)
  const elapsed = duration !== null && duration > 1_000 ? formatDuration(duration) : ""
  const glyph = row.status === "awaiting_approval" ? "?" : row.status === "running"
    ? SPINNER[spinnerFrame % SPINNER.length]!
    : row.isError ? "✗" : "✓"
  const outcome = row.status === "awaiting_approval" ? `${glyph} awaiting approval` : elapsed === "" ? glyph : `${glyph} ${elapsed}`
  const color = row.status === "awaiting_approval" ? theme.warning : row.status === "running" ? theme.info
    : row.isError ? theme.error : theme.success
  const rowWidth = Math.max(20, availableWidth - 1)
  const verb = truncateToCells(summary.verb, 32)
  const detail = truncateToCells(summary.detail, 40)
  const fixed = 2 + cellWidth(verb) + cellWidth(outcome) + (detail === "" ? 0 : cellWidth(detail) + 2) + 2
  const subject = summary.subject === "" ? "" : truncateToCells(summary.subject, Math.max(8, rowWidth - fixed - 1))
  const left = `● ${verb}${subject === "" ? "" : ` ${subject}`}${detail === "" ? "" : `  ${detail}`}`
  const spacing = " ".repeat(Math.max(2, rowWidth - cellWidth(left) - cellWidth(outcome)))
  const detailColor = row.status === "finished" && row.isError ? theme.error : theme.textMuted
  return t`${fg(color)("● ")}${fg(theme.text)(verb)}${subject === "" ? "" : fg(theme.textMuted)(` ${subject}`)}${detail === "" ? "" : fg(detailColor)(`  ${detail}`)}${spacing}${fg(color)(outcome)}`
}

export function formatDuration(milliseconds: number): string {
  if (milliseconds < 60_000) return `${(milliseconds / 1_000).toFixed(1)}s`
  const minutes = Math.floor(milliseconds / 60_000)
  const seconds = Math.floor((milliseconds % 60_000) / 1_000)
  return `${minutes}m${seconds.toString().padStart(2, "0")}s`
}

function builtinVerb(name: string): string {
  return ({ read: "Read", write: "Write", edit: "Edit", multi_edit: "Edit", bash: "Bash", grep: "Search", glob: "Find", ls: "List" } as Record<string, string>)[name] ?? name
}

function failureDetail(row: ToolRow): string {
  if (row.display.permissionDenied) return "denied"
  const exit = /^Exit code · (-?\d+)$/m.exec(row.display.details)?.[1]
  if (exit !== undefined && exit !== "0") return `exit ${exit}`
  return oneLine(row.display.summary === "Failed" ? "failed" : row.display.summary)
}

/** Counts come from the tool's presentation fields or a small complete JSON result. */
function resultCount(row: ToolRow): number | null {
  if (row.status !== "finished" || row.isError) return null
  const labeled = /^(?:Matches|Files|Entries|Results) · (\d+)$/m.exec(row.display.details)?.[1]
  if (labeled !== undefined) return Number(labeled)
  const details = row.display.details.trimStart()
  if (!details.startsWith("{") || row.display.truncated) return null
  try {
    const parsed: unknown = JSON.parse(details)
    if (typeof parsed === "object" && parsed !== null && "count" in parsed && typeof parsed.count === "number") return parsed.count
  } catch { /* Plain text results have no count. */ }
  return null
}

function agentSummary(args: ToolRow["args"], summary: (verb: string, subject: string, detail?: string) => ToolSummary): ToolSummary {
  const action = typeof args?.action === "string" ? args.action : "spawn"
  const id = typeof args?.id === "string" ? args.id : ""
  switch (action) {
    case "wait": {
      const count = Array.isArray(args?.ids) ? args.ids.length : 0
      return summary("Wait for agents", `${count} ${count === 1 ? "agent" : "agents"}`)
    }
    case "message": return summary("Message agent", id)
    case "cancel": return summary("Cancel agent", id)
    case "close": return summary("Close agent", id)
    case "list": return summary("List agents", "")
    default: {
      const agent = typeof args?.agent === "string" ? args.agent : "general"
      const task = typeof args?.task === "string" ? firstLine(oneLine(args.task)) : ""
      return summary("Agent", `${agent}${task === "" ? "" : ` ${quoted(task)}`}`)
    }
  }
}

function todoSubject(args: ToolRow["args"]): string {
  switch (typeof args?.action === "string" ? args.action : "") {
    case "replace": {
      const count = Array.isArray(args?.items) ? args.items.length : 0
      return `${count} ${count === 1 ? "item" : "items"}`
    }
    case "upsert": {
      const item = args?.item
      const title = typeof item === "object" && item !== null
        ? Reflect.get(item, "title") ?? Reflect.get(item, "content") ?? Reflect.get(item, "text") : undefined
      return typeof title === "string" ? quoted(oneLine(title)) : "1 item"
    }
    case "remove": return typeof args?.id === "string" ? `remove ${args.id}` : "remove item"
    case "clear": return "clear all"
    case "list": return "list"
    default: return ""
  }
}

function position(path: string, args: ToolRow["args"]): string {
  const line = typeof args?.line === "number" ? args.line : null
  return line === null ? path : `${path}:${line}`
}

/** The first short string argument, never a whole serialized object. */
function firstString(value: unknown): string {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return ""
  let visited = 0
  for (const key in value) {
    if (!Object.hasOwn(value, key) || ++visited > 8) break
    if (/token|secret|password|authorization|api[_-]?key|credential/i.test(key)) continue
    const candidate = Reflect.get(value, key)
    if (typeof candidate === "string" && candidate.trim() !== "") return firstLine(oneLine(candidate))
  }
  const fallback = formatToolSubject(value)
  return fallback.includes("=") ? "" : fallback
}

function quoted(value: string): string {
  return value === "" ? "" : `"${truncateToCells(value, 48)}"`
}

function firstLine(value: string): string {
  return value.split("\n", 1)[0]?.trim() ?? ""
}

function oneLine(value: string): string {
  return value.replace(/[\u0000-\u0009\u000b-\u001f\u007f-\u009f‪-‮⁦-⁩]/g, " ")
    .replace(/[ \t]+/g, " ").trim()
}
