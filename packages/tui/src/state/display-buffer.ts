import { MAX_PENDING_TOOL_INVOCATIONS, TRANSCRIPT_TAIL_TEXT_BYTES } from "../../../../protocol/types"
import type { ToolOutputStream } from "../protocol"

// The admitted live invocation set shares a fixed preview payload allowance.
export const MAX_ACTIVE_TOOL_DISPLAY_BYTES = 8 * 1024 * 1024
export const MAX_TOOL_DISPLAY_BYTES = Math.floor(MAX_ACTIVE_TOOL_DISPLAY_BYTES / MAX_PENDING_TOOL_INVOCATIONS)
export const MAX_TOOL_DISPLAY_CHUNKS = 1025
export const MAX_TAIL_TEXT_BYTES = TRANSCRIPT_TAIL_TEXT_BYTES
export const LIVE_OUTPUT_TRUNCATION_MARKER = "[live tool output truncated; command output continues to drain]"
const MAX_WINDOW_LINES = 32
export const MAX_PREVIEW_LINE_CODE_UNITS = 4096
export const PREVIEW_LINE_TRUNCATION_MARKER = "[earlier characters omitted] "
export const DISPLAY_TRUNCATION_MARKER = "[additional output omitted from this client display]"

export function utf8Prefix(value: string, maxBytes: number): string {
  if (maxBytes <= 0) return ""
  if (Buffer.byteLength(value) <= maxBytes) return value
  let low = 0
  let high = Math.min(value.length, maxBytes)
  while (low < high) {
    const middle = Math.ceil((low + high) / 2)
    if (Buffer.byteLength(value.slice(0, middle)) <= maxBytes) low = middle
    else high = middle - 1
  }
  const last = value.charCodeAt(low - 1)
  if (last >= 0xd800 && last <= 0xdbff) low -= 1
  return Buffer.from(value.slice(0, low), "utf8").toString("utf8")
}

export interface ToolOutputChunk {
  readonly stream: ToolOutputStream
  readonly chunk: string
}
interface ChunkNode {
  readonly previous: ChunkNode | null
  readonly value: ToolOutputChunk
}
export interface OutputLineWindow {
  readonly lines: readonly string[]
  readonly lineCount: number
}
interface LineWindow extends OutputLineWindow {
  readonly visibleLines: readonly string[]
  readonly visibleLineCount: number
  readonly endedWithCR: boolean
}
export interface ToolOutputView {
  readonly plain: string
  readonly plainWindow: OutputLineWindow
  readonly labeledWindow: OutputLineWindow
  readonly labeled: string
  readonly tailLines: readonly string[]
  readonly lineCount: number
  readonly sourceTruncated: boolean
}
interface Materialization extends ToolOutputView {
  readonly node: ChunkNode | null
  readonly count: number
  readonly window: LineWindow
  readonly markerTail: string
}
interface StreamCache {
  current: Materialization
  visitedNodes: number
  windowInputCodeUnits: number
}
const materializations = new WeakMap<object, StreamCache>()
const truncatedMaterializations = new WeakMap<Materialization, ToolOutputView>()
const EMPTY_WINDOW: LineWindow = { lines: [""], visibleLines: [], lineCount: 1, visibleLineCount: 0, endedWithCR: false }
const EMPTY_MATERIALIZATION: Materialization = {
  node: null, count: 0, plain: "", labeled: "", tailLines: [], lineCount: 0, sourceTruncated: false,
  markerTail: "", window: EMPTY_WINDOW, plainWindow: EMPTY_WINDOW, labeledWindow: EMPTY_WINDOW,
}

function boundedPreviewLine(value: string): string {
  if (value.length <= MAX_PREVIEW_LINE_CODE_UNITS) return value
  let start = value.length - (MAX_PREVIEW_LINE_CODE_UNITS - PREVIEW_LINE_TRUNCATION_MARKER.length)
  const first = value.charCodeAt(start)
  if (first >= 0xdc00 && first <= 0xdfff) start += 1
  // Copy the bounded suffix so a preview cannot retain the full temporary concatenation.
  return PREVIEW_LINE_TRUNCATION_MARKER + Buffer.from(value.slice(start), "utf8").toString("utf8")
}

function appendWindow(current: LineWindow, text: string): LineWindow {
  if (text === "") return current
  const normalized = (current.endedWithCR && text.startsWith("\n") ? text.slice(1) : text)
    .replaceAll("\r\n", "\n").replaceAll("\r", "\n")
  const lines = [...current.lines]
  let visibleLines = current.visibleLines
  let lineCount = current.lineCount
  let visibleLineCount = current.visibleLineCount
  const parts = normalized.split("\n")
  const lastContentPart = parts.findLastIndex((part) => part !== "")
  for (let index = 0; index < parts.length; index += 1) {
    const part = parts[index] ?? ""
    if (index > 0) {
      lines.push("")
      lineCount += 1
      if (lines.length > MAX_WINDOW_LINES) lines.shift()
    }
    lines[lines.length - 1] = boundedPreviewLine((lines.at(-1) ?? "") + part)
    if (index === lastContentPart) {
      visibleLines = [...lines]
      visibleLineCount = lineCount
    }
  }
  return { lines, visibleLines, lineCount, visibleLineCount, endedWithCR: text.endsWith("\r") }
}

function appendRawWindow(current: OutputLineWindow, text: string): OutputLineWindow {
  if (text === "") return current
  const parts = text.split("\n")
  const lines = [...current.lines]
  lines[lines.length - 1] = boundedPreviewLine((lines.at(-1) ?? "") + (parts[0] ?? ""))
  for (let index = 1; index < parts.length; index += 1) {
    lines.push(boundedPreviewLine(parts[index] ?? ""))
    if (lines.length > MAX_WINDOW_LINES) lines.shift()
  }
  return { lines, lineCount: current.lineCount + parts.length - 1 }
}

/** Immutable input history; one weakly owned materialization per live stream, not per prefix. */
export class ToolOutputBuffer {
  private constructor(
    readonly count = 0,
    readonly retainedBytes = 0,
    readonly omittedBytes = 0,
    readonly truncated = false,
    private readonly root: object = {},
    private readonly node: ChunkNode | null = null,
  ) {}

  static empty(): ToolOutputBuffer { return new ToolOutputBuffer() }

  /** A source preview cannot accept later bytes across an omitted region. */
  static fromPreview(text: string, truncated: boolean): ToolOutputBuffer {
    const initial = ToolOutputBuffer.empty().append({ stream: "stdout", chunk: text })
    return new ToolOutputBuffer(initial.count, initial.retainedBytes, initial.omittedBytes, initial.truncated || truncated, initial.root, initial.node)
  }

  append(value: ToolOutputChunk): ToolOutputBuffer {
    const bytes = Buffer.byteLength(value.chunk)
    const remaining = Math.max(0, MAX_TOOL_DISPLAY_BYTES - this.retainedBytes)
    const allowed = !this.truncated && this.count < MAX_TOOL_DISPLAY_CHUNKS
    const chunk = allowed ? utf8Prefix(value.chunk, remaining) : ""
    const retained = Buffer.byteLength(chunk)
    const truncated = this.truncated || !allowed || retained < bytes
    const root = this.count === 0 && !this.truncated ? {} : this.root
    return new ToolOutputBuffer(
      this.count + (allowed ? 1 : 0),
      this.retainedBytes + retained,
      Math.min(Number.MAX_SAFE_INTEGER, this.omittedBytes + bytes - retained),
      truncated,
      root,
      allowed ? { previous: this.node, value: { stream: value.stream, chunk } } : this.node,
    )
  }

  read(): ToolOutputView {
    let cache = materializations.get(this.root)
    if (cache === undefined) {
      cache = { current: EMPTY_MATERIALIZATION, visitedNodes: 0, windowInputCodeUnits: 0 }
      materializations.set(this.root, cache)
    }
    let result = cache.current
    if (result.node !== this.node) {
      const pending: ToolOutputChunk[] = []
      let cursor = this.node
      while (cursor !== null && cursor !== result.node) {
        pending.push(cursor.value)
        cache.visitedNodes += 1
        cursor = cursor.previous
      }
      if (cursor !== result.node) result = EMPTY_MATERIALIZATION
      let plain = result.plain
      let labeled = result.labeled
      let plainWindow = result.plainWindow
      let labeledWindow = result.labeledWindow
      let window = result.window
      let sourceTruncated = result.sourceTruncated
      let markerTail = result.markerTail
      for (let index = pending.length - 1; index >= 0; index -= 1) {
        const value = pending[index]
        if (value === undefined) continue
        window = appendWindow(window, value.chunk)
        const markerCandidate = markerTail + value.chunk
        sourceTruncated ||= markerCandidate.includes(LIVE_OUTPUT_TRUNCATION_MARKER)
        markerTail = markerCandidate.slice(-(LIVE_OUTPUT_TRUNCATION_MARKER.length - 1))
        const labeledChunk = `${labeled === "" ? "" : "\n"}${value.stream === "stderr" ? "Error output" : "Output"}\n${value.chunk.trimEnd()}`
        plainWindow = appendRawWindow(plainWindow, value.chunk)
        labeledWindow = appendRawWindow(labeledWindow, labeledChunk)
        cache.windowInputCodeUnits += value.chunk.length * 2 + labeledChunk.length
        plain += value.chunk
        labeled += labeledChunk
      }
      result = { node: this.node, count: this.count, plain, labeled, plainWindow, labeledWindow, window, markerTail,
        tailLines: window.visibleLines, lineCount: window.visibleLineCount, sourceTruncated }
      // An old immutable snapshot must not roll the forward materialization cursor back.
      if (this.count >= cache.current.count) cache.current = result
    }
    if (!this.truncated) return result
    const existing = truncatedMaterializations.get(result)
    if (existing !== undefined) return existing
    const view: ToolOutputView = {
      ...result, sourceTruncated: true,
      plainWindow: appendRawWindow(result.plainWindow, `\n${DISPLAY_TRUNCATION_MARKER}`),
      labeledWindow: appendRawWindow(result.labeledWindow, `\n${DISPLAY_TRUNCATION_MARKER}`),
      plain: `${result.plain}\n${DISPLAY_TRUNCATION_MARKER}`,
      labeled: `${result.labeled}\n${DISPLAY_TRUNCATION_MARKER}`,
    }
    truncatedMaterializations.set(result, view)
    return view
  }

  get materializationWork(): { readonly visitedNodes: number; readonly retainedVersions: number; readonly windowInputCodeUnits: number } {
    const cache = materializations.get(this.root)
    return { visitedNodes: cache?.visitedNodes ?? 0, retainedVersions: cache === undefined ? 0 : 1, windowInputCodeUnits: cache?.windowInputCodeUnits ?? 0 }
  }

  /** Render projections are not wire or recycle state; diagnostics must not recurse through chain history. */
  toJSON(): { readonly count: number; readonly retainedBytes: number; readonly omittedBytes: number; readonly truncated: boolean } {
    return { count: this.count, retainedBytes: this.retainedBytes, omittedBytes: this.omittedBytes, truncated: this.truncated }
  }
}

export const EMPTY_TOOL_OUTPUT = ToolOutputBuffer.empty()

export function toolOutputBuffer(chunks: readonly ToolOutputChunk[]): ToolOutputBuffer {
  return chunks.reduce((buffer, chunk) => buffer.append(chunk), EMPTY_TOOL_OUTPUT)
}

export function boundedUtf8(value: string, maxBytes: number): string {
  if (Buffer.byteLength(value) <= maxBytes) return value
  if (maxBytes < 3) return ".".repeat(Math.max(0, maxBytes))
  return `${utf8Prefix(value, maxBytes - 3)}…`
}
