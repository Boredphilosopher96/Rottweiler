import type { ToolProjection } from "../state"
import { formatToolSubject } from "../tool-arguments"

export interface ToolPresentation {
  readonly subject: string
  readonly summary: string
  readonly details: string
}


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

export function presentTool(tool: ToolProjection): ToolPresentation {
  const presentation = tool.display ?? {
    subject: formatToolSubject(tool.args),
    summary: tool.status === "awaiting_approval" ? "Awaiting approval" : "Running",
    details: "",
  }
  const subject = displayPath(presentation.subject)
  return subject === presentation.subject ? presentation : { ...presentation, subject }
}
