import type { TranscriptContent, TranscriptContentSource } from "../protocol"
import type { ToolProjection, ToolStatus } from "../state"
import { prepareToolDisplay, type ToolDisplay } from "../state/tool-display"
import { diffStats, presentableUnifiedDiff } from "./diff"
import { presentTool } from "./tool-presentation"

export type TranscriptToolContent = Extract<TranscriptContent, { type: "tool" }>

/** Where the complete output of a row can be opened. */
export type ToolOutputTarget =
  | { readonly type: "live"; readonly invocationId: string }
  | { readonly type: "source"; readonly source: TranscriptContentSource }

/**
 * The single presentation model for a tool invocation. Live projections and
 * restored transcript items both reduce to this shape, so one renderer draws
 * every tool row the same way before and after a turn completes.
 */
export interface ToolRow {
  readonly invocationId: string
  readonly name: string
  /** Structured arguments, or null when the durable preview cannot be interpreted. */
  readonly args: Readonly<Record<string, unknown>> | null
  readonly argumentsNote: string | null
  readonly status: ToolStatus
  readonly isError: boolean
  readonly display: ToolDisplay
  readonly diff: { readonly path: string; readonly unifiedDiff: string } | null
  readonly diffStats: { readonly added: number; readonly removed: number } | null
  readonly diffSource: TranscriptContentSource | null
  readonly rationale: string | null
  /** Wall-clock bounds when known; running rows measure against the current time. */
  readonly startedAtMs: number | null
  readonly finishedAtMs: number | null
  readonly output: ToolOutputTarget | null
  /** Title of a structured result view that the output target opens. */
  readonly presentationTitle: string | null
  /** Live running output is read from the projection's bounded chunk buffer. */
  readonly live: ToolProjection | null
}

export type ToolRowSource = ToolProjection | TranscriptToolContent

export function toolRow(source: ToolRowSource): ToolRow {
  return "invocationId" in source ? liveToolRow(source) : transcriptToolRow(source)
}

export function liveToolRow(tool: ToolProjection): ToolRow {
  const args = isRecord(tool.args) ? tool.args : tool.display === null ? null : recoveredArguments(tool.name, tool.display)
  const display = tool.display ?? {
    ...presentTool(tool), truncated: false, permissionDenied: false,
    command: args !== null && typeof args.command === "string" ? args.command : null,
  }
  const diff = isRecord(tool.diff) && typeof tool.diff.path === "string" && typeof tool.diff.unified_diff === "string"
    ? { path: tool.diff.path, unifiedDiff: presentableUnifiedDiff(tool.diff.path, tool.diff.unified_diff) }
    : null
  const timing = tool.timing
  return {
    invocationId: tool.invocationId,
    name: tool.name,
    args,
    argumentsNote: null,
    status: tool.status,
    isError: tool.isError === true,
    display,
    diff,
    diffStats: diff === null ? null : diffStats(diff.unifiedDiff),
    diffSource: tool.diffSource,
    rationale: tool.rationale,
    startedAtMs: timing.kind === "unknown" ? null : timing.startedAtMs,
    finishedAtMs: timing.kind === "closed" ? timing.finishedAtMs : null,
    output: { type: "live", invocationId: tool.invocationId },
    presentationTitle: null,
    live: tool,
  }
}

const MAX_HISTORY_ARGUMENT_BYTES = 4096

export function transcriptToolRow(content: TranscriptToolContent): ToolRow {
  const preview = content.arguments
  let args: Record<string, unknown> | null = null
  const clipped = !preview.complete || Buffer.byteLength(preview.text) > MAX_HISTORY_ARGUMENT_BYTES
  if (!clipped && preview.format === "json") {
    try {
      const parsed: unknown = JSON.parse(preview.text)
      if (isRecord(parsed)) args = parsed
    } catch { /* Retain explicit unavailable copy; never infer from partial JSON. */ }
  }
  const finished = content.status.type === "finished" ? content.status : null
  const isError = finished?.is_error === true
  const output = finished?.output ?? null
  const prepared = prepareToolDisplay({ type: "text", text: output?.text.slice(0, 4096) ?? "" }, null, args, isError)
  const display: ToolDisplay = {
    ...prepared,
    truncated: prepared.truncated || output?.complete === false,
    summary: finished === null ? "Running" : (!isError ? finished.presentation?.title : null) ?? prepared.summary,
  }
  const diff = transcriptDiff(content.diff, typeof args?.path === "string" ? args.path : "")
  return {
    invocationId: content.invocation_id,
    name: content.name,
    args,
    argumentsNote: args === null ? clipped ? "arguments truncated" : "arguments unavailable" : null,
    status: finished === null ? "running" : "finished",
    isError,
    display,
    diff,
    diffStats: diff === null ? null : diffStats(diff.unifiedDiff),
    diffSource: content.diff !== null && diff === null ? content.diff.source : null,
    rationale: null,
    startedAtMs: null,
    finishedAtMs: null,
    output: finished === null ? null
      : { type: "source", source: (!isError ? finished.presentation?.source : null) ?? finished.output.source },
    presentationTitle: !isError ? finished?.presentation?.title ?? null : null,
    live: null,
  }
}

const PRIMARY_ARGUMENT: Readonly<Record<string, string>> = {
  read: "path", write: "path", edit: "path", multi_edit: "path", ls: "path", diagnostics: "path",
  bash: "command", grep: "pattern", glob: "pattern", symbols: "pattern", webfetch: "url", websearch: "query",
  skill: "name", ask_user: "question", tool_search: "query",
}

/**
 * Finished live projections release their structured arguments and keep only
 * the prepared display scalars. Rebuild the few named values a summary needs
 * from those scalars (`Path=a.rs · Old=…` or a lone value) so a row that is
 * first drawn after completion reads the same as one drawn while running.
 */
function recoveredArguments(name: string, display: ToolDisplay): Record<string, unknown> | null {
  const args: Record<string, unknown> = {}
  if (display.command !== null) args.command = display.command
  const subject = display.subject.trim()
  if (/^[A-Z][\w ]{0,63}=/.test(subject)) {
    for (const part of subject.split(" · ")) {
      const separator = part.indexOf("=")
      if (separator <= 0) continue
      const key = part.slice(0, separator).toLowerCase().replaceAll(" ", "_")
      if (!(key in args)) args[key] = part.slice(separator + 1)
    }
  } else if (subject !== "") {
    const key = PRIMARY_ARGUMENT[name]
    if (key !== undefined && !(key in args)) args[key] = subject
  }
  return Object.keys(args).length === 0 ? null : args
}

/** A durable diff preview is the proposal's JSON; only a complete one is drawn inline. */
function transcriptDiff(preview: TranscriptToolContent["diff"], fallbackPath: string): ToolRow["diff"] {
  if (preview === null || !preview.complete) return null
  let path = fallbackPath
  let text = preview.text
  if (preview.format === "json") {
    try {
      const parsed: unknown = JSON.parse(preview.text)
      if (!isRecord(parsed) || typeof parsed.unified_diff !== "string") return null
      if (typeof parsed.path === "string") path = parsed.path
      text = parsed.unified_diff
    } catch { return null }
  }
  return { path, unifiedDiff: presentableUnifiedDiff(path, text) }
}

/** Elapsed milliseconds, or null when the row has no trustworthy start time. */
export function toolDurationMs(row: ToolRow, nowMs: number): number | null {
  if (row.startedAtMs === null) return null
  const end = row.status === "finished" || row.status === "completed" ? row.finishedAtMs : nowMs
  return end === null ? null : Math.max(0, end - row.startedAtMs)
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}
