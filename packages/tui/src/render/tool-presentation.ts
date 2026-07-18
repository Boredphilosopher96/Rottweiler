import type { ToolProjection } from "../state"
import {
  formatToolArguments,
  structuredToolSummary,
  toolPlainText,
  toolStructuredData,
} from "./format"

export interface ToolPresentation {
  readonly subject: string
  readonly summary: string
  readonly details: string
}

type Presenter = (tool: ToolProjection, data: unknown, text: string) => ToolPresentation

let workspaceRoots: readonly string[] = []

export function setWorkspaceRoots(roots: readonly string[]): void {
  workspaceRoots = [...roots]
}

export function displayPath(path: string): string {
  const root = workspaceRoots
    .map((candidate) => candidate.replace(/[\\/]+$/, ""))
    .filter((candidate) => candidate !== "" && (path.startsWith(`${candidate}/`) || path.startsWith(`${candidate}\\`)))
    .sort((left, right) => right.length - left.length)[0]
  return root === undefined ? path : path.slice(root.length + 1)
}

const PRESENTERS: Readonly<Record<string, Presenter>> = {
  read: presentRead,
  write: presentWrite,
  edit: presentEdit,
  multi_edit: presentEdit,
  apply_worktree_diff: presentEdit,
  ls: presentList,
  glob: presentSearch,
  grep: presentSearch,
  search: presentSearch,
  symbols: presentSearch,
  diagnostics: presentDiagnostics,
  definition: presentLocations,
  references: presentLocations,
  rename: presentLocations,
  bash: presentBash,
  shell: presentBash,
  websearch: presentWebSearch,
  webfetch: presentWebFetch,
  todo: presentTodo,
  ask_user: presentAnswer,
  submit_plan: presentPlan,
  background_status: presentBackground,
  background_output: presentBackground,
  background_kill: presentBackground,
  spawn_agent: presentSubagent,
  tool_search: presentToolSearch,
  mcp_call: presentMcp,
}

export function presentTool(tool: ToolProjection): ToolPresentation {
  const data = toolStructuredData(tool.output)
  const text = toolPlainText(tool.output).trim()
  if (tool.isError === true) return presentFailure(tool, data, text)
  const presenter = PRESENTERS[tool.name] ?? (tool.name.startsWith("mcp__") ? presentMcp : presentGeneric)
  const result = presenter(tool, data, text)
  return withDiffNotice(tool, result)
}

function presentRead(tool: ToolProjection, data: unknown, text: string): ToolPresentation {
  const payload = record(data)
  const path = displayPath(string(payload?.path) || pathArgument(tool))
  const total = integer(payload?.total_lines) ?? (text === "" ? null : text.split("\n").length)
  const start = integer(payload?.start_line)
  const bytes = integer(payload?.bytes)
  const metrics = [
    total === null ? "" : `${total} line${total === 1 ? "" : "s"}`,
    bytes === null ? "" : formatBytes(bytes),
  ].filter(Boolean).join(" · ")
  return {
    subject: path,
    summary: metrics || "Read",
    details: joinLines(
      path === "" ? "" : `File · ${path}`,
      start === null ? metrics : `From line ${start} · ${metrics}`,
      payload === null ? text : "",
    ),
  }
}

function presentWrite(tool: ToolProjection, data: unknown): ToolPresentation {
  const payload = record(data)
  const path = displayPath(string(payload?.path) || pathArgument(tool))
  const bytes = integer(payload?.bytes)
  return {
    subject: path,
    summary: bytes === null ? "File written" : `${formatBytes(bytes)} written`,
    details: joinLines(path === "" ? "" : `File · ${path}`, bytes === null ? "File written" : `${formatBytes(bytes)} written`),
  }
}

function presentEdit(tool: ToolProjection, data: unknown): ToolPresentation {
  const payload = record(data)
  const path = displayPath(string(payload?.path) || pathArgument(tool))
  const args = record(tool.args)
  const count = integer(payload?.edits) ?? (Array.isArray(args?.edits) ? args.edits.length : 1)
  const noun = `${count} change${count === 1 ? "" : "s"}`
  return {
    subject: path,
    summary: noun,
    details: joinLines(path === "" ? "" : `File · ${path}`, `${noun} applied`),
  }
}

function presentList(tool: ToolProjection, data: unknown, text: string): ToolPresentation {
  const payload = record(data)
  const entries = array(payload?.entries)
  const structuredRows = entries.flatMap((value) => {
    const item = record(value)
    const path = string(item?.path)
    if (path === "") return []
    const kind = human(string(item?.kind) || "item")
    return [`${kind} · ${displayPath(path)}`]
  })
  const rows = structuredRows.length > 0 ? structuredRows : text.split("\n").map((line) => displayPath(line.trim())).filter(Boolean)
  const count = integer(payload?.count) ?? rows.length
  const path = displayPath(pathArgument(tool) || ".")
  return {
    subject: path,
    summary: count === 0 ? "No entries" : `${count} item${count === 1 ? "" : "s"}`,
    details: rows.length === 0 ? `Directory · ${path}\nNo entries.` : `Directory · ${path}\n${boundedRows(rows, "entries")}`,
  }
}

function presentSearch(tool: ToolProjection, data: unknown, text: string): ToolPresentation {
  const payload = record(data)
  const values = Array.isArray(payload?.paths)
    ? payload.paths
    : Array.isArray(payload?.matches)
      ? payload.matches
      : []
  const structuredRows = values.flatMap((value) => {
    if (typeof value === "string") return [displayPath(value)]
    const item = record(value)
    if (item === null) return []
    const path = string(item.path)
    const location = integer(item.line) ?? integer(record(item.location)?.line)
    const label = string(item.text) || string(item.name)
    if (path === "" && label === "") return []
    return [`${displayPath(path)}${location === null ? "" : `:${location}`} ${label}`.trim()]
  })
  const rows = structuredRows.length > 0 ? structuredRows : text.split("\n").map((line) => displayPath(line.trim())).filter(Boolean)
  const count = integer(payload?.count) ?? rows.length
  const subject = searchSubject(tool)
  const empty = tool.name === "glob" ? "No matching files" : "No matches"
  return {
    subject,
    summary: count === 0 ? empty : `${count} ${tool.name === "glob" ? "file" : "match"}${count === 1 ? "" : tool.name === "glob" ? "s" : "es"}`,
    details: rows.length === 0 ? `${empty}.` : boundedRows(rows, tool.name === "glob" ? "files" : "matches"),
  }
}

function presentDiagnostics(tool: ToolProjection, data: unknown): ToolPresentation {
  const payload = record(data)
  const diagnostics = array(payload?.diagnostics)
  const rows = diagnostics.flatMap((value) => {
    const item = record(value)
    if (item === null) return []
    const path = displayPath(string(item.path) || pathArgument(tool))
    const start = record(record(item.range)?.start)
    const line = integer(start?.line)
    const character = integer(start?.character)
    const location = line === null ? path : `${path}:${line + 1}:${(character ?? 0) + 1}`
    const severity = human(string(item.severity) || "diagnostic")
    const message = string(item.message) || "Diagnostic reported"
    return [`${severity} · ${location} · ${message}`]
  })
  const count = rows.length
  return {
    subject: displayPath(pathArgument(tool)),
    summary: count === 0 ? "No diagnostics" : `${count} diagnostic${count === 1 ? "" : "s"}`,
    details: count === 0 ? "No diagnostics." : boundedRows(rows, "diagnostics"),
  }
}

function presentLocations(tool: ToolProjection, data: unknown): ToolPresentation {
  const payload = record(data)
  const key = tool.name === "definition" ? "definitions" : tool.name === "references" ? "references" : "edits"
  const values = array(payload?.[key])
  const rows = values.flatMap((value) => {
    const item = record(value)
    if (item === null) return []
    const path = displayPath(string(item.path))
    const start = record(record(item.range)?.start)
    const line = integer(start?.line)
    const character = integer(start?.character)
    return path === "" ? [] : [`${path}${line === null ? "" : `:${line + 1}:${(character ?? 0) + 1}`}`]
  })
  return {
    subject: displayPath(pathArgument(tool)),
    summary: rows.length === 0 ? `No ${key}` : `${rows.length} ${key}`,
    details: rows.length === 0 ? `No ${key}.` : boundedRows(rows, key),
  }
}

function presentBash(tool: ToolProjection, data: unknown, text: string): ToolPresentation {
  const payload = record(data)
  const exitCode = integer(payload?.exit_code)
  const captured = parseBashResult(text)
  const live = tool.chunks.map((chunk) => `${chunk.stream === "stderr" ? "Error output" : "Output"}\n${chunk.chunk.trimEnd()}`).join("\n")
  const details = captured === null
    ? live || (text === "" ? "Completed with no output." : text)
    : joinLines(
        captured.stdout === "" ? "" : `Output\n${captured.stdout}`,
        captured.stderr === "" ? "" : `Error output\n${captured.stderr}`,
      ) || "Completed with no output."
  return {
    subject: "",
    summary: exitCode === null || exitCode === 0 ? "Completed" : `exit ${exitCode}`,
    details,
  }
}

function presentWebSearch(tool: ToolProjection, data: unknown): ToolPresentation {
  const payload = record(data)
  const results = array(payload?.results)
  const rows = results.flatMap((value) => {
    const item = record(value)
    if (item === null) return []
    const title = string(item.title) || "Result"
    const url = string(item.url)
    const snippet = string(item.snippet)
    return [joinLines(`${title}${url === "" ? "" : ` · ${url}`}`, snippet)]
  })
  const count = integer(payload?.count) ?? rows.length
  return {
    subject: searchSubject(tool),
    summary: count === 0 ? "No results" : `${count} result${count === 1 ? "" : "s"}`,
    details: rows.length === 0 ? "No results." : boundedRows(rows, "results"),
  }
}

function presentWebFetch(tool: ToolProjection, data: unknown): ToolPresentation {
  const payload = record(data)
  const url = string(payload?.final_url) || string(record(tool.args)?.url)
  const status = integer(payload?.status)
  const bytes = integer(payload?.bytes)
  const metrics = [status === null ? "" : `HTTP ${status}`, bytes === null ? "" : formatBytes(bytes)].filter(Boolean).join(" · ")
  return { subject: url, summary: metrics || "Fetched", details: joinLines(url === "" ? "" : `URL · ${url}`, metrics) }
}

function presentTodo(_tool: ToolProjection, data: unknown): ToolPresentation {
  const payload = record(data)
  const items = array(payload?.items)
  const rows = items.flatMap((value) => {
    const item = record(value)
    if (item === null) return []
    const content = string(item.content)
    if (content === "") return []
    const status = string(item.status)
    const glyph = status === "completed" ? "✓" : status === "in_progress" ? "◌" : status === "blocked" ? "!" : "○"
    return [`${glyph} ${content}`]
  })
  return {
    subject: "",
    summary: `${rows.length} todo${rows.length === 1 ? "" : "s"}`,
    details: rows.length === 0 ? "No todos." : boundedRows(rows, "todos"),
  }
}

function presentAnswer(tool: ToolProjection, data: unknown): ToolPresentation {
  const answer = string(record(data)?.answer)
  const question = string(record(tool.args)?.question)
  return { subject: question, summary: answer === "" ? "Answered" : singleLine(answer, 48), details: joinLines(question, answer) }
}

function presentPlan(tool: ToolProjection, data: unknown): ToolPresentation {
  const payload = record(data)
  const title = string(payload?.title) || string(record(tool.args)?.title)
  const steps = array(payload?.steps)
  return { subject: title, summary: `${steps.length} step${steps.length === 1 ? "" : "s"} submitted`, details: title === "" ? "Plan submitted." : `Plan · ${title}` }
}

function presentBackground(tool: ToolProjection, data: unknown): ToolPresentation {
  const payload = record(data)
  const process = record(payload?.process)
  const processes = array(payload?.processes)
  const id = string(process?.process_id) || string(record(tool.args)?.process_id)
  const status = human(string(process?.status))
  const count = processes.length
  const summary = count > 0 ? `${count} process${count === 1 ? "" : "es"}` : status || (tool.name === "background_kill" ? "Process stopped" : "Completed")
  return { subject: id, summary, details: joinLines(id === "" ? "" : `Process · ${id}`, summary) }
}

function presentSubagent(tool: ToolProjection, data: unknown): ToolPresentation {
  const payload = record(data)
  const status = human(string(payload?.status))
  const action = human(string(payload?.action))
  const task = string(record(tool.args)?.task)
  const files = array(payload?.touched_files)
  const summary = status || (action === "" ? "Child updated" : `${action} complete`)
  return { subject: task, summary, details: joinLines(task === "" ? "" : `Task · ${task}`, summary, files.length === 0 ? "" : `${files.length} changed file${files.length === 1 ? "" : "s"}`) }
}

function presentToolSearch(tool: ToolProjection, data: unknown): ToolPresentation {
  const payload = record(data)
  const count = integer(payload?.count) ?? array(payload?.tools).length
  return { subject: searchSubject(tool), summary: count === 0 ? "No tools found" : `${count} tool${count === 1 ? "" : "s"}`, details: count === 0 ? "No tools found." : `${count} matching tool${count === 1 ? "" : "s"}.` }
}

function presentMcp(tool: ToolProjection, data: unknown): ToolPresentation {
  const payload = record(data)
  const server = string(payload?.server)
  const operation = string(payload?.operation) || tool.name.replace(/^mcp__/, "").replaceAll("__", " · ")
  const format = human(string(payload?.format))
  const overflow = payload?.overflow === true
  return {
    subject: [server, operation].filter(Boolean).join(" · "),
    summary: overflow ? "Completed · more output available" : "Completed",
    details: joinLines(server === "" ? "" : `Server · ${server}`, operation === "" ? "" : `Tool · ${operation}`, format === "" ? "" : `Format · ${format}`, overflow ? "More output is available." : ""),
  }
}

function presentGeneric(tool: ToolProjection, data: unknown, text: string): ToolPresentation {
  const subject = formatToolArguments(tool.args, 80)
  const structured = data === null ? "" : structuredToolSummary(data)
  const safeText = /^[\[{]/.test(text.trim()) ? "" : text
  const details = structured || safeText || "Completed."
  return { subject, summary: firstLine(details, 56) || "Completed", details }
}

function presentFailure(tool: ToolProjection, _data: unknown, text: string): ToolPresentation {
  let message = text
  if (/remembered_permission_unavailable/i.test(message)) {
    message = "This command can only be approved once. Choose Allow once to continue."
  } else if (/error parsing diff:|line count did not match for hunk/i.test(message)) {
    message = "Couldn't apply the requested change."
  } else if (/^permission denied for tool/i.test(message)) {
    message = "Permission denied. The tool was not run."
  }
  message = message.trim() || "The tool did not complete."
  return {
    subject: tool.name === "bash" || tool.name === "shell" ? "" : formatToolArguments(tool.args, 80),
    summary: firstLine(message, 56),
    details: `Error\n${message}`,
  }
}

function withDiffNotice(tool: ToolProjection, presentation: ToolPresentation): ToolPresentation {
  const diff = record(tool.diff)
  const source = string(diff?.unified_diff)
  if (source === "") return presentation
  const lines = source.split("\n").length
  if (lines <= 12) return presentation
  return { ...presentation, details: joinLines(`Diff preview · showing 12 of ${lines} lines · ctrl+r to review`, presentation.details) }
}

function parseBashResult(text: string): { stdout: string; stderr: string } | null {
  const match = /^exit code:\s*[^\n]+\nstdout:\n([\s\S]*?)\nstderr:\n([\s\S]*)$/i.exec(text)
  return match === null ? null : { stdout: (match[1] ?? "").trimEnd(), stderr: (match[2] ?? "").trimEnd() }
}

function pathArgument(tool: ToolProjection): string {
  return string(record(tool.args)?.path) || string(record(tool.args)?.file_path) || string(record(tool.args)?.filePath)
}

function searchSubject(tool: ToolProjection): string {
  const args = record(tool.args)
  return string(args?.query) || string(args?.pattern)
}

function record(value: unknown): Record<string, unknown> | null {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : null
}

function array(value: unknown): readonly unknown[] {
  return Array.isArray(value) ? value : []
}

function string(value: unknown): string {
  return typeof value === "string" ? value.trim() : ""
}

function integer(value: unknown): number | null {
  return typeof value === "number" && Number.isSafeInteger(value) ? value : null
}

function human(value: string): string {
  return value.replaceAll("_", " ").replace(/\b\w/g, (letter) => letter.toUpperCase())
}

function firstLine(value: string, limit: number): string {
  return singleLine(value.split("\n").find((line) => line.trim() !== "") ?? "", limit)
}

function singleLine(value: string, limit: number): string {
  const compact = value.replace(/\s+/g, " ").trim()
  return compact.length <= limit ? compact : `${compact.slice(0, Math.max(1, limit - 1))}…`
}

function boundedRows(rows: readonly string[], noun: string, limit = 7): string {
  return rows.length <= limit ? rows.join("\n") : [...rows.slice(0, limit), `… ${rows.length - limit} more ${noun}`].join("\n")
}

function joinLines(...values: readonly string[]): string {
  return values.filter((value) => value.trim() !== "").join("\n")
}

function formatBytes(bytes: number): string {
  if (bytes < 1_024) return `${bytes} B`
  if (bytes < 1_048_576) return `${(bytes / 1_024).toFixed(bytes < 10_240 ? 1 : 0)} KB`
  return `${(bytes / 1_048_576).toFixed(1)} MB`
}
