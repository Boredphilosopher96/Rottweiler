// This local bridge targets the dependency's terminal-only implementation so
// Bun does not bundle its unrelated SVG/ELK renderer into the standalone TUI.
import { renderMermaidASCII } from "./beautiful-mermaid-ascii.js"

const MERMAID_LANGUAGES = new Set(["mermaid", "mmd", "flowchart"])
const MAX_DIAGRAM_BYTES = 32 * 1024
const MAX_DIAGRAM_LINES = 320
const MAX_DIAGRAM_EDGES = 256
const MAX_DIAGRAM_STATEMENTS = 384
const MAX_DIAGRAM_GROUP_SEPARATORS = 128
const MAX_DIAGRAMS_PER_RESPONSE = 8
const MAX_RESPONSE_DIAGRAM_BYTES = 128 * 1024
const MAX_RESPONSE_DIAGRAM_EDGES = 1_024
const MAX_RESPONSE_DIAGRAM_STATEMENTS = 1_024
const MAX_RENDERED_BYTES = 128 * 1024
// Height is handled by the transcript's scroll viewport. This is only a guard
// against a broken renderer producing an unbounded allocation.
const MAX_RENDERED_LINES = 2_048
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
      ? diagramFallback(diagramWidth, metrics, sourceTooLarge)
      : exceedsResponseBudget
        ? diagramFallback(diagramWidth, metrics)
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
    return diagramFallback(width, metrics)
  }
  if (metrics.edges > 48 && !isChunkableFlowchart(source)) {
    return diagramFallback(width, metrics)
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

  let rendered: string | null = renderChunkedFlowchart(source, requestedWidth, metrics)
  for (const candidate of rendered === null ? diagramCandidates(source, layoutWidth, metrics) : []) {
    try {
      const picture = renderMermaidASCII(candidate.source, {
        useAscii: false,
        colorMode: "none",
        ...candidate.padding,
      }).trimEnd()
      if (picture.trim().length === 0) continue
      const withLegend = candidate.legend.length === 0
        ? picture
        : `${picture}\n\n${renderLegend(candidate.legend, requestedWidth)}`
      const fitted = fitRenderedDiagram(withLegend, requestedWidth)
      if (fitted !== null) {
        rendered = fitted
        break
      }
    } catch {
      // Some Mermaid layouts are unsupported by the terminal renderer. Keep
      // trying narrower layouts rather than leaking parser internals.
    }
  }
  rendered ??= diagramFallback(requestedWidth, metrics, false, true)

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

function diagramFallback(
  width: number,
  metrics: DiagramMetrics,
  sourceTruncated = false,
  renderFailed = false,
): string {
  const columns = Math.max(1, Math.floor(width))
  const reason = renderFailed
    ? "This Mermaid syntax is not supported by the terminal renderer."
    : sourceTruncated
      ? "This Mermaid source exceeds the safe rendering limit."
      : "This diagram exceeds the safe rendering limit."
  const details = metrics.edges > 0
    ? `${metrics.edges} connection${metrics.edges === 1 ? "" : "s"}`
    : `${metrics.statements} statement${metrics.statements === 1 ? "" : "s"}`
  return ["◇ Mermaid diagram", reason, details]
    .map((line) => clipTerminalLine(line, columns))
    .join("\n")
}

interface DiagramCandidate {
  readonly source: string
  readonly padding: {
    readonly paddingX: number
    readonly paddingY: number
    readonly boxBorderPadding: number
  }
  readonly legend: readonly [string, string][]
}

function diagramCandidates(source: string, width: number, metrics: DiagramMetrics): readonly DiagramCandidate[] {
  const normal = {
    paddingX: width < 80 ? 1 : 3,
    paddingY: width < 80 ? 1 : 2,
    boxBorderPadding: width < 48 ? 0 : 1,
  }
  const tight = { paddingX: 1, paddingY: 1, boxBorderPadding: 0 }
  const flush = { paddingX: 0, paddingY: 0, boxBorderPadding: 0 }
  const vertical = verticalFlowchart(source)
  const compact = compactFlowchartLabels(vertical)
  const candidates: DiagramCandidate[] = metrics.edges > 16
    ? [{ source: vertical, padding: flush, legend: [] }]
    : [
        { source, padding: normal, legend: [] },
        { source, padding: tight, legend: [] },
        { source: vertical, padding: flush, legend: [] },
      ]
  if (compact.source !== vertical) {
    candidates.push({ source: compact.source, padding: flush, legend: compact.legend })
  }
  return candidates
}

function renderChunkedFlowchart(
  source: string,
  width: number,
  metrics: DiagramMetrics,
): string | null {
  if (metrics.edges <= 16 || !isChunkableFlowchart(source)) return null
  const lines = verticalFlowchart(source).split("\n")
  const header = lines.find((line) => /^\s*(?:flowchart|graph)\s+/i.test(line)) ?? "flowchart TB"
  const edges = lines.filter(hasMermaidEdge)
  for (const pageSize of [8, 4, 2, 1]) {
    const pictures: string[] = []
    let failed = false
    for (let index = 0; index < edges.length; index += pageSize) {
      const chunkSource = [header, ...edges.slice(index, index + pageSize)].join("\n")
      try {
        const picture = renderMermaidASCII(chunkSource, {
          useAscii: false,
          colorMode: "none",
          paddingX: 0,
          paddingY: 0,
          boxBorderPadding: 0,
        }).trimEnd()
        if (picture === "") {
          failed = true
          break
        }
        const page = Math.floor(index / pageSize) + 1
        const pages = Math.ceil(edges.length / pageSize)
        const title = clipTerminalLine(`◇ Diagram ${page} of ${pages}`, width)
        pictures.push(`${title}\n${picture}`)
      } catch {
        failed = true
        break
      }
    }
    if (failed) continue
    const fitted = fitRenderedDiagram(pictures.join("\n\n"), width)
    if (fitted !== null) return fitted
  }
  return null
}

function isFlowchartHeader(line: string): boolean {
  return /^\s*(?:flowchart|graph)\s+/i.test(line)
}

function hasMermaidEdge(line: string): boolean {
  return /(?:--+>|--{2,}|==+>|-\.->|<--?>|->>|-->>|\.{2,}>)/.test(line)
}

const SIMPLE_FLOWCHART_EDGE = /^\s*([A-Za-z_][\w.-]*)\s*(?:--+>|==+>|-\.->|->>|-->>|\.{2,}>)\s*([A-Za-z_][\w.-]*)\s*;?\s*$/

function isChunkableFlowchart(source: string): boolean {
  let edges = 0
  for (const line of source.split("\n")) {
    if (line.trim() === "" || isFlowchartHeader(line) || /^\s*%%/.test(line)) continue
    if (!SIMPLE_FLOWCHART_EDGE.test(line)) return false
    edges += 1
  }
  return edges > 0
}

function verticalFlowchart(source: string): string {
  return source.replace(/^(\s*(?:flowchart|graph)\s+)(?:LR|RL)(\b)/im, "$1TB$2")
}

function compactFlowchartLabels(source: string): {
  readonly source: string
  readonly legend: readonly [string, string][]
} {
  const labels = new Map<string, string>()
  const compact = source.replace(
    /\b([A-Za-z_][\w.-]*)\s*\[(?![\[(])([^\][\n]*)\]/g,
    (match, identifier: string, rawLabel: string) => {
      const shapeLabel = rawLabel.trim()
      if (/^[\\/]|[\\/]$/.test(shapeLabel)) return match
      const label = cleanMermaidLabel(rawLabel)
      if (label !== "" && label !== identifier && !labels.has(identifier)) labels.set(identifier, label)
      return `${identifier}[${identifier}]`
    },
  )
  return { source: compact, legend: [...labels] }
}

function cleanMermaidLabel(label: string): string {
  return label
    .replace(/<br\s*\/?\s*>/gi, " / ")
    .replace(/^["'`]|["'`]$/g, "")
    .replace(/\s+/g, " ")
    .trim()
}

function renderLegend(entries: readonly [string, string][], width: number): string {
  const lines = ["Legend"]
  for (const [identifier, label] of entries) {
    lines.push(...wrapTerminalLine(`${identifier}: ${label}`, width))
  }
  return lines.join("\n")
}

function wrapTerminalLine(line: string, width: number): string[] {
  const columns = Math.max(1, Math.floor(width))
  if (Bun.stringWidth(line) <= columns) return [line]
  const words = line.split(/\s+/)
  const output: string[] = []
  let current = ""
  for (const word of words) {
    const candidate = current === "" ? word : `${current} ${word}`
    if (Bun.stringWidth(candidate) <= columns) {
      current = candidate
      continue
    }
    if (current !== "") output.push(current)
    current = Bun.stringWidth(word) <= columns ? word : clipTerminalLine(word, columns)
  }
  if (current !== "") output.push(current)
  return output
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
