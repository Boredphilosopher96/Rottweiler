const MERMAID_LANGUAGES = new Set(["mermaid", "mmd", "flowchart"])

/**
 * Keep Mermaid honest in a character-cell terminal.
 *
 * The previous path attempted to reinterpret Mermaid as an ASCII graph. That
 * changed layouts, dropped syntax, and occasionally exposed renderer failures
 * as assistant output. OpenTUI has no reliable Mermaid surface, so retain the
 * authored source and mark the fence as plain text. A future image-capable
 * surface can render the original Mermaid without changing this transcript.
 */
export function terminalMarkdown(
  markdown: string,
  _width: number,
  _phase: "streaming" | "complete" = "complete",
): string {
  if (!markdown.includes("```") && !markdown.includes("~~~")) return markdown
  return markdown
    .split("\n")
    .map(normalizeMermaidFence)
    .join("\n")
}

function normalizeMermaidFence(line: string): string {
  const match = /^(.*?)(`{3,}|~{3,})\s*([^\s`~]+)(.*)$/.exec(line)
  if (match === null) return line
  const language = (match[3] ?? "").toLowerCase()
  if (!MERMAID_LANGUAGES.has(language)) return line
  return `${match[1] ?? ""}${match[2] ?? "```"}text`
}
