import type { ToolOutput, UiPresentation } from "../protocol"
import { formatToolSubject } from "../tool-arguments"
import { boundedUtf8 } from "./display-buffer"

export const MAX_TOOL_RESULT_PREVIEW_BYTES = 4096
export interface ToolDisplay {
  readonly subject: string
  readonly summary: string
  readonly details: string
  readonly truncated: boolean
  readonly permissionDenied: boolean
  readonly command: string | null
}

/** Prepare scalar display values once; frames never inspect complete result bodies. */
export function prepareToolDisplay(output: ToolOutput, presentation: UiPresentation | null, args: unknown, isError: boolean): ToolDisplay {
  try { return prepare(output, presentation, args, isError) }
  catch {
    return { subject: "", summary: "Presentation unavailable", details: "The tool presentation is invalid. Open full output.", truncated: true, permissionDenied: false, command: null }
  }
}
function prepare(output: ToolOutput, presentation: UiPresentation | null, args: unknown, isError: boolean): ToolDisplay {
  let details = ""
  let truncated = false
  let summary = isError ? "Failed" : "Completed"
  const append = (value: string) => {
    const available = MAX_TOOL_RESULT_PREVIEW_BYTES - Buffer.byteLength(details)
    const line = boundedUtf8(value, Math.max(0, available))
    details += line
    truncated ||= line !== value
  }
  if (presentation !== null && !isError) {
    summary = presentation.descriptor.title
    const values = new Map(presentation.projected.fields.map(field => [field.id, field]))
    if (values.size !== presentation.descriptor.fields.length || values.size !== presentation.projected.fields.length
      || new Set(presentation.descriptor.fields.map(field => field.id)).size !== values.size) throw new Error("tool presentation field count mismatch")
    for (const field of presentation.descriptor.fields) {
      const value = values.get(field.id)
      if (value === undefined || value.kind !== field.kind) throw new Error("tool presentation field identity mismatch")
      if (details !== "") append("\n")
      append(field.label)
      switch (value.kind) {
        case "text": case "badge":
          append(` · ${value.value ?? "—"}`)
          break
        case "list":
          for (const item of value.values) { if (truncated) break; append(`\n• ${item}`) }
          break
        case "table":
          for (const row of value.rows) {
            if (truncated) break
            for (let index = 0; index < row.length; index++) {
              if (truncated) break
              append(`${index === 0 ? "\n" : " │ "}${row[index] || "—"}`)
            }
          }
          break
      }
      if (truncated) break
    }
    truncated ||= presentation.projected.truncated
  } else {
    const text = (value: string) => {
      // Protected model framing is not a display contract.
      if (/^\s*<rottweiler_untrusted_/.test(value.slice(0, 256))) return
      if (details !== "") append("\n")
      append(value)
    }
    if (output.type === "text") text(output.text)
    else if (output.type === "mixed") {
      for (let index = 0; index < output.parts.length; index++) {
        if (index >= 32) { truncated = true; break }
        const part = output.parts[index]!
        if (truncated) break
        if (part.type === "text") text(part.text)
        else if (part.type === "image") append(`Image · ${part.media_type}`)
      }
    }
    if (details === "") details = isError ? "The tool did not complete." : "Result available in full output."
  }
  details = details.replaceAll("\r\n", "\n").replaceAll("\r", "\n")
    .replace(/\u001b\[[0-?]*[ -/]*[@-~]/g, "")
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/g, "")
  const permissionDenied = isError && /^permission denied for tool/i.test(details.trimStart())
  if (permissionDenied) details = "Permission denied. The tool was not run."
  else if (isError && /remembered_permission_unavailable/i.test(details)) details = "This command can only be approved once. Choose Allow once to continue."
  else if (isError && /error parsing diff:|line count did not match for hunk/i.test(details)) details = "Couldn't apply the requested change."
  if (isError) summary = details.split("\n", 1)[0]?.slice(0, 80) || "Failed"
  const command = args !== null && typeof args === "object" && "command" in args && typeof args.command === "string"
    ? boundedUtf8(args.command, MAX_TOOL_RESULT_PREVIEW_BYTES) : null
  return { subject: formatToolSubject(args), summary, details, truncated, permissionDenied, command }
}
