import { fg, t } from "@opentui/core"
import type { TranscriptContent } from "../protocol"
import type { ToolProjection } from "../state"
import { prepareToolDisplay } from "../state/tool-display"
import type { RottweilerTheme } from "../theme"
import { presentTool } from "./tool-presentation"
import { truncateToCells } from "./text"

export type ToolHeaderInput = Pick<ToolProjection, "name" | "args" | "status" | "isError" | "display">
export function compactToolPresentation(tool: ToolHeaderInput): { subject: string; summary: string } {
  const presentation = presentTool(tool)
  return {
    subject: truncateToCells(presentation.subject.replace(/\s+/g, " ").trim(), 80),
    summary: truncateToCells(presentation.summary.replace(/\s+/g, " ").trim(), 56),
  }
}

/** Both storage paths render the same header from bounded scalar presentation. */
export function toolHeaderContent(tool: ToolHeaderInput, collapsed: boolean, availableWidth: number, theme: RottweilerTheme, elapsed = ""): ReturnType<typeof t> {
  const compact = compactToolPresentation(tool)
  const result = tool.status === "finished" && collapsed ? compact.summary : ""
  const glyph = tool.status === "awaiting_approval" ? "?" : tool.status === "running" ? "◌" : tool.isError === true ? "✕" : "✓"
  const outcome = tool.status === "awaiting_approval"
    ? `${glyph} approval needed` : result === "" ? `${glyph}${elapsed}` : `${glyph} ${result}${elapsed}`
  const color = tool.status === "awaiting_approval" ? theme.warning : tool.isError === true ? theme.error : tool.status === "finished" ? theme.success : theme.info
  const rowWidth = Math.max(20, availableWidth - 3)
  const toolName = truncateToCells(tool.name.replaceAll("_", "-"), 12)
  const subject = truncateToCells(result !== "" && compact.subject !== "" && result.toLowerCase().includes(compact.subject.toLowerCase())
    ? "" : compact.subject, Math.max(0, rowWidth - toolName.length - outcome.length - 7))
  const name = `${toolName}${subject === "" ? "" : "  "}`
  const indicator = collapsed ? "▸" : "⌄"
  const prefix = `${indicator} ${name}${subject}`
  const spacing = " ".repeat(Math.max(2, rowWidth - prefix.length - outcome.length))
  return t`${fg(theme.textMuted)(`${indicator} `)}${fg(theme.secondary)(name)}${subject === "" ? "" : fg(theme.text)(subject)}${spacing}${fg(color)(outcome)}`
}

export function historicalToolPresentation(content: Extract<TranscriptContent, { type: "tool" }>): ToolHeaderInput {
  const preview = content.arguments
  let args: unknown = null
  const clipped = !preview.complete || Buffer.byteLength(preview.text) > 4096
  if (!clipped && preview.format === "json") {
    try {
      const parsed: unknown = JSON.parse(preview.text)
      if (typeof parsed === "object" && parsed !== null && !Array.isArray(parsed)) args = parsed
    } catch { /* Retain explicit unavailable copy; never infer from partial JSON. */ }
  }
  const status = content.status.type
  const isError = status === "finished" && content.status.type === "finished" && content.status.is_error
  const output = content.status.type === "finished" ? content.status.output : null
  const display = prepareToolDisplay({ type: "text", text: output?.text.slice(0, 4096) ?? "" }, null, args, isError)
  return { name: content.name, args, status, isError, display: {
    ...display,
    subject: args === null ? clipped ? "arguments truncated" : "arguments unavailable" : display.subject,
    summary: content.status.type === "running" ? "Running" : (!isError ? content.status.presentation?.title : null) ?? display.summary,
  } }
}
