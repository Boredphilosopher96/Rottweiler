// This local bridge targets the dependency's terminal-only implementation so
// Bun does not bundle its unrelated SVG/ELK renderer into the standalone TUI.
import { renderMermaidASCII } from "./beautiful-mermaid-ascii.js"

const MERMAID_LANGUAGES = new Set(["mermaid", "mmd", "flowchart"])
const MAX_DIAGRAM_BYTES = 8 * 1024
const MAX_DIAGRAM_LINES = 80
const MAX_DIAGRAM_EDGES = 24
const MAX_DIAGRAM_STATEMENTS = 40
const MAX_DIAGRAM_GROUP_SEPARATORS = 24
const MAX_DIAGRAMS_PER_RESPONSE = 4
const MAX_RESPONSE_DIAGRAM_BYTES = 16 * 1024
const MAX_RESPONSE_DIAGRAM_EDGES = 32
const MAX_RESPONSE_DIAGRAM_STATEMENTS = 64
const MAX_RENDERED_BYTES = 32 * 1024
const MAX_RENDERED_LINES = 24
const MAX_CACHE_ENTRIES = 64
const MAX_CACHE_BYTES = 256 * 1024

const renderedDiagramCache = new Map<string, string>()
let renderedDiagramCacheBytes = 0

interface DiagramMetrics {
  readonly bytes: number
  readonly lines: number
  readonly statements: number
  readonly edges: number
  readonly groupSeparators: number
}

interface FenceOpening {
  readonly prefix: string
  readonly fence: string
  readonly language: string
}

/**
 * Replace Mermaid Markdown fences with terminal-native Unicode diagrams.
 *
 * Completed fences render synchronously, so there is no source-then-diagram
 * flash. An unfinished streaming fence is replaced by a stable progress row
 * until its closing fence arrives. Invalid or unreasonably large diagrams are
 * presented as a concise user-facing fallback instead of parser internals.
 */
export function terminalMarkdown(
  markdown: string,
  width: number,
  phase: "streaming" | "complete" = "complete",
): string {
  if (!markdown.includes("```") && !markdown.includes("~~~")) return markdown
  const lines = markdown.split("\n")
  const output: string[] = []
  let renderedDiagrams = 0
  let responseDiagramBytes = 0
  let responseDiagramEdges = 0
  let responseDiagramStatements = 0

  for (let index = 0; index < lines.length; index += 1) {
    const opening = parseFenceOpening(lines[index] ?? "")
    if (opening === null || !MERMAID_LANGUAGES.has(opening.language)) {
      output.push(lines[index] ?? "")
      continue
    }

    const fence = opening.fence
    const fenceCharacter = fence[0] ?? "`"
    const source: string[] = []
    let collectedBytes = 0
    let totalSourceLines = 0
    let totalStatements = 0
    let totalEdges = 0
    let totalGroupSeparators = 0
    let sourceTooLarge = false
    let closing = -1
    let containerEnded = -1
    for (let cursor = index + 1; cursor < lines.length; cursor += 1) {
      const line = lines[cursor] ?? ""
      const sourceLine = stripContainerPrefix(line, opening.prefix)
      if (sourceLine === null) {
        containerEnded = cursor
        break
      }
      const closingFence = parseClosingFence(sourceLine)
      if (
        closingFence !== undefined &&
        closingFence.fence[0] === fenceCharacter &&
        closingFence.fence.length >= fence.length
      ) {
        closing = cursor
        break
      }
      collectedBytes += new TextEncoder().encode(sourceLine).length + 1
      totalSourceLines += 1
      const lineMetrics = diagramLineMetrics(sourceLine)
      totalStatements += lineMetrics.statements
      totalEdges += lineMetrics.edges
      totalGroupSeparators += lineMetrics.groupSeparators
      if (source.length >= MAX_DIAGRAM_LINES || collectedBytes > MAX_DIAGRAM_BYTES) {
        sourceTooLarge = true
      } else {
        source.push(sourceLine)
      }
    }

    if (closing < 0) {
      const quote = opening.prefix.includes(">") ? opening.prefix : `${opening.prefix}> `
      const message = sourceTooLarge
        ? `${quote}◌ Preparing compact diagram…`
        : phase === "streaming"
          ? `${quote}◌ Rendering diagram…`
          : `${quote}Diagram is incomplete because its closing fence is missing.`
      output.push(clipTerminalLine(message, Math.max(1, width)))
      if (containerEnded < 0) break
      index = containerEnded - 1
      continue
    }

    const sourceText = source.join("\n")
    const metrics = sourceTooLarge
      ? {
          bytes: Math.max(0, collectedBytes - 1),
          lines: totalSourceLines,
          statements: totalStatements,
          edges: totalEdges,
          groupSeparators: totalGroupSeparators,
        }
      : diagramMetrics(sourceText)
    const prefixWidth = Bun.stringWidth(opening.prefix)
    if (prefixWidth + Bun.stringWidth("```text") > width) {
      output.push(clipTerminalLine(`${opening.prefix}◇ Diagram`, Math.max(1, width)))
      index = closing
      continue
    }
    const exceedsDiagramBudget = diagramExceedsLimits(metrics)
    const exceedsResponseBudget =
      renderedDiagrams >= MAX_DIAGRAMS_PER_RESPONSE ||
      responseDiagramBytes + metrics.bytes > MAX_RESPONSE_DIAGRAM_BYTES ||
      responseDiagramEdges + metrics.edges > MAX_RESPONSE_DIAGRAM_EDGES ||
      responseDiagramStatements + metrics.statements > MAX_RESPONSE_DIAGRAM_STATEMENTS
    const diagramWidth = Math.max(1, width - prefixWidth)
    const rendered = sourceTooLarge || exceedsDiagramBudget
      ? compactDiagramPreview(sourceText, diagramWidth, metrics, sourceTooLarge)
      : exceedsResponseBudget
        ? compactDiagramPreview(sourceText, diagramWidth, metrics)
        : renderDiagram(sourceText, diagramWidth, metrics)
    if (!sourceTooLarge && !exceedsDiagramBudget && !exceedsResponseBudget) {
      renderedDiagrams += 1
      responseDiagramBytes += metrics.bytes
      responseDiagramEdges += metrics.edges
      responseDiagramStatements += metrics.statements
    }
    output.push(`${opening.prefix}\`\`\`text`)
    output.push(...rendered.split("\n").map((line) => `${opening.prefix}${line}`))
    output.push(`${opening.prefix}\`\`\``)
    index = closing
  }

  return output.join("\n")
}

function renderDiagram(source: string, width: number, metrics: DiagramMetrics): string {
  if (diagramExceedsLimits(metrics)) {
    return compactDiagramPreview(source, width, metrics)
  }

  const requestedWidth = Math.max(1, Math.floor(width))
  const layoutWidth = Math.max(20, requestedWidth)
  const cacheKey = `${requestedWidth}\u0000${source}`
  const cached = renderedDiagramCache.get(cacheKey)
  if (cached !== undefined) {
    renderedDiagramCache.delete(cacheKey)
    renderedDiagramCache.set(cacheKey, cached)
    return cached
  }

  let rendered: string
  try {
    rendered = renderMermaidASCII(source, {
      useAscii: false,
      colorMode: "none",
      paddingX: layoutWidth < 80 ? 1 : 3,
      paddingY: layoutWidth < 80 ? 1 : 2,
      boxBorderPadding: layoutWidth < 48 ? 0 : 1,
    }).trimEnd()
    if (rendered.trim().length === 0) rendered = "Diagram has no visible nodes."
    const fitted = fitRenderedDiagram(rendered, requestedWidth)
    rendered = fitted ?? compactDiagramPreview(source, requestedWidth, metrics)
  } catch {
    rendered = compactDiagramPreview(source, requestedWidth, metrics)
  }

  const renderedBytes = new TextEncoder().encode(rendered).length
  renderedDiagramCache.set(cacheKey, rendered)
  renderedDiagramCacheBytes += renderedBytes
  while (
    renderedDiagramCache.size > MAX_CACHE_ENTRIES ||
    renderedDiagramCacheBytes > MAX_CACHE_BYTES
  ) {
    const oldest = renderedDiagramCache.entries().next().value as [string, string] | undefined
    if (oldest === undefined) break
    renderedDiagramCache.delete(oldest[0])
    renderedDiagramCacheBytes -= new TextEncoder().encode(oldest[1]).length
  }
  return rendered
}

function diagramExceedsLimits(metrics: DiagramMetrics): boolean {
  return metrics.bytes > MAX_DIAGRAM_BYTES ||
    metrics.lines > MAX_DIAGRAM_LINES ||
    metrics.statements > MAX_DIAGRAM_STATEMENTS ||
    metrics.edges > MAX_DIAGRAM_EDGES ||
    metrics.groupSeparators > MAX_DIAGRAM_GROUP_SEPARATORS
}

function fitRenderedDiagram(rendered: string, width: number): string | null {
  const maxColumns = Math.max(1, Math.floor(width))
  const sourceLines = rendered.split("\n")
  if (sourceLines.length > MAX_RENDERED_LINES) return null
  if (sourceLines.some((line) => Bun.stringWidth(line) > maxColumns)) return null
  if (new TextEncoder().encode(rendered).length > MAX_RENDERED_BYTES) return null
  return rendered
}

function compactDiagramPreview(
  source: string,
  width: number,
  metrics: DiagramMetrics,
  sourceTruncated = false,
): string {
  const columns = Math.max(1, Math.floor(width))
  const header = metrics.edges > 0
    ? `◇ Compact diagram · ${metrics.edges} connection${metrics.edges === 1 ? "" : "s"}${sourceTruncated ? " · partial preview" : ""}`
    : `◇ Compact diagram${sourceTruncated ? " · partial preview" : ""}`
  const statements = source
    .split(/\n|;/)
    .map(compactDiagramStatement)
    .filter((line): line is string => line !== null)
  const unique = [...new Set(statements)]
  const bodyLimit = Math.max(1, MAX_RENDERED_LINES - 1)
  const visible = unique.slice(0, bodyLimit)
  if (sourceTruncated && visible.length > 0) {
    visible[visible.length - 1] = "… diagram preview truncated"
  } else if (unique.length > visible.length) {
    visible[visible.length - 1] = `… ${unique.length - visible.length + 1} more connections`
  }
  const body = visible.length === 0 ? ["No visible connections"] : visible
  return [header, ...body]
    .slice(0, MAX_RENDERED_LINES)
    .map((line) => clipTerminalLine(line, columns))
    .join("\n")
}

function compactDiagramStatement(statement: string): string | null {
  const trimmed = statement.trim()
  if (
    trimmed === "" ||
    /^(?:%%|flowchart\b|graph\b|subgraph\b|end$|direction\b|style\b|classDef\b|class\b|click\b|linkStyle\b)/i.test(trimmed)
  ) return null
  const compact = trimmed
    .replace(/<br\s*\/?\s*>/gi, " ")
    .replace(/\|[^|]*\|/g, " ")
    .replace(/([A-Za-z_][\w.-]*)\s*\[([^\]]+)]/g, "$2")
    .replace(/([A-Za-z_][\w.-]*)\s*\(([^)]+)\)/g, "$2")
    .replace(/([A-Za-z_][\w.-]*)\s*\{([^}]+)}/g, "$2")
    .replace(/(?:--+>|--{2,}|==+>|-\.->|<--?>|->>|-->>|\.{2,}>)/g, " → ")
    .replace(/["'`]/g, "")
    .replace(/\s+/g, " ")
    .trim()
  return compact === "" ? null : compact
}

function clipTerminalLine(line: string, width: number): string {
  if (Bun.stringWidth(line) <= width) return line
  const target = Math.max(1, width - 1)
  let result = ""
  let cells = 0
  for (const character of line) {
    const characterWidth = Bun.stringWidth(character)
    if (cells + characterWidth > target) break
    result += character
    cells += characterWidth
  }
  return `${result}…`
}

function parseFenceOpening(line: string): FenceOpening | null {
  const match = /^((?:(?: {0,3}>[ \t]?)+)? {0,3})(`{3,}|~{3,})(.*)$/.exec(line)
  if (match === null) return null
  const prefix = match[1] ?? ""
  const fence = match[2] ?? ""
  const info = (match[3] ?? "").trim()
  if (fence.startsWith("`") && info.includes("`")) return null
  const language = info.split(/\s+/, 1)[0]?.toLocaleLowerCase() ?? ""
  return { prefix, fence, language }
}

function parseClosingFence(line: string): { readonly fence: string } | undefined {
  const match = /^((?:(?: {0,3}>[ \t]?)+)? {0,3})(`{3,}|~{3,})\s*$/.exec(line)
  const fence = match?.[2]
  return fence === undefined ? undefined : { fence }
}

function stripContainerPrefix(line: string, prefix: string): string | null {
  if (prefix === "") return line
  const expectedQuotes = prefix.match(/>/g)?.length ?? 0
  if (expectedQuotes > 0) {
    const match = /^((?: {0,3}>[ \t]?)+)(.*)$/.exec(line)
    if ((match?.[1]?.match(/>/g)?.length ?? 0) !== expectedQuotes) return null
    return match?.[2] ?? ""
  }
  return line.startsWith(prefix) ? line.slice(prefix.length) : null
}

function diagramMetrics(source: string): DiagramMetrics {
  const lines = source.split("\n")
  let edges = 0
  let statements = 0
  let groupSeparators = 0
  for (const line of lines) {
    const metrics = diagramLineMetrics(line)
    statements += metrics.statements
    edges += metrics.edges
    groupSeparators += metrics.groupSeparators
  }
  return {
    bytes: new TextEncoder().encode(source).length,
    lines: lines.length,
    statements,
    edges,
    groupSeparators,
  }
}

function diagramLineMetrics(line: string): Pick<DiagramMetrics, "statements" | "edges" | "groupSeparators"> {
  const statements = line.split(";").filter((part) => part.trim() !== "").length
  const groupSeparators = line.match(/&/g)?.length ?? 0
  const segments = line.split(/(?:--+>|--{2,}|==+>|-\.->|<--?>|->>|-->>|\.{2,}>)/)
  let edges = 0
  for (let index = 0; index < segments.length - 1; index += 1) {
    const leftGroups = (segments[index]?.match(/&/g)?.length ?? 0) + 1
    const rightGroups = (segments[index + 1]?.match(/&/g)?.length ?? 0) + 1
    edges += leftGroups * rightGroups
  }
  return { statements, edges, groupSeparators }
}
