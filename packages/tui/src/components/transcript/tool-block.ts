import { TextRenderable } from "../text"
import type { ToolOutputText } from "../../state/output-reader"
import { bindSelectableClick } from "../selectable-click"
import type { TranscriptContentSource } from "../../protocol"
import {
  BoxRenderable,
  CodeRenderable,
  DiffRenderable,
  type RenderContext,
  type SyntaxStyle,
  type TreeSitterClient,
} from "@opentui/core"
import {
  commandPreview,
  COMMAND_PREVIEW_MAX_LINES,
  filetypeForPath,
  minimalUnifiedDiff,
  splitDiffVisualRows,
  unifiedDiffVisualRows,
  presentTool,
  truncateToCells,
  truncateUnifiedDiff,
} from "../../render"
import { TOOL_SPINNER_INTERVAL_MS, toolHeaderContent } from "../../render/tool-header"
import { toolRow, type ToolRow, type ToolRowSource } from "../../render/tool-row"
import type { ToolProjection } from "../../state"
import type { RottweilerTheme } from "../../theme"

const MAX_PREVIEW_LINES = 8
const MAX_INLINE_DIFF_ROWS = 24
const HEADER_COMMAND_CELLS = 60

/** Actions a tool row can take on behalf of the user. */
export interface ToolBlockRendering {
  readonly syntaxStyle: SyntaxStyle
  readonly treeSitterClient?: TreeSitterClient
  readonly onOpenToolOutput?: (invocationId: string) => void
  /** Opens source-backed content of a live projection. */
  readonly onOpenLiveContent?: (source: TranscriptContentSource) => void
  /** Opens source-backed content of a restored transcript page. */
  readonly onOpenContent?: (source: TranscriptContentSource) => void
}

/**
 * The one tool-row renderer. Live projections and restored transcript items
 * are reduced to the same `ToolRow`, so a row looks identical while running,
 * awaiting approval, finished, and after the session is reopened.
 *
 * Collapsed: one line — status bullet, humanized summary, outcome.
 * Expanded: the command (when the header cannot show all of it), the diff for
 * edits, and a bounded output preview. A clickable marker opens the complete
 * output only when the preview omits content. Approval previews are owned by
 * the approval dock, so a pending row never duplicates the diff or command.
 */
export class ToolBlockRenderable extends BoxRenderable {
  readonly header: TextRenderable
  readonly body: TextRenderable
  readonly truncationMarker: TextRenderable
  command: CodeRenderable | TextRenderable | null = null
  diff: DiffRenderable | TextRenderable | null = null
  commandPrompt: TextRenderable | null = null
  blockId: string
  #commandContainer: BoxRenderable | null = null
  #diffContainer: BoxRenderable | null = null
  readonly #bodyContainer: BoxRenderable
  #commandSignature = ""
  #diffSignature = ""
  #headerSignature = ""
  #collapsed: boolean
  #userSetExpansion: boolean
  #selected = false
  #availableWidth: number
  #row: ToolRow | null
  #source: ToolRowSource | null
  #spinner: ReturnType<typeof setInterval> | null = null
  #spinnerFrame = 0
  #rootsGeneration = ""
  #lastRender: { readonly source: ToolRowSource; readonly width: number; readonly collapsed: boolean; readonly rootsGeneration: string } | null = null
  readonly #theme: RottweilerTheme
  #onExpansionChange: ((expanded: boolean) => void) | undefined
  readonly #rendering: ToolBlockRendering | undefined

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    source: ToolRowSource,
    expanded?: boolean,
    onExpansionChange?: (expanded: boolean) => void,
    rendering?: ToolBlockRendering,
  ) {
    const row = toolRow(source)
    const blockId = `tool:${row.invocationId}`
    super(ctx, {
      id: `tool-${row.invocationId}`,
      width: "100%",
      minHeight: 1,
      flexDirection: "column",
      border: false,
      backgroundColor: theme.background,
      // Expansion is mouse-driven while keyboard focus remains owned by the
      // transcript scroller/composer. Individual tool rows must not trap it.
      focusable: false,
      paddingX: 0,
      marginTop: 0,
    })
    this.blockId = blockId
    this.#theme = theme
    this.#row = row
    this.#source = source
    this.#availableWidth = Math.max(20, ctx.width)
    this.#collapsed = expanded === undefined ? !expandsByDefault(row) : !expanded
    this.#userSetExpansion = expanded !== undefined
    this.#onExpansionChange = onExpansionChange
    this.#rendering = rendering
    this.header = new TextRenderable(ctx, {
      id: `${blockId}:header`, content: "", fg: theme.text, bg: theme.background,
      width: "100%", height: 1, selectable: true,
    })
    this.body = new TextRenderable(ctx, {
      content: "", fg: theme.textMuted, wrapMode: "word", visible: false, selectable: true,
    })
    this.truncationMarker = new TextRenderable(ctx, {
      content: "", fg: theme.textMuted, height: 0, flexShrink: 0, wrapMode: "none", visible: false, selectable: true,
    })
    this.#bodyContainer = new BoxRenderable(ctx, {
      id: `tool-body-${row.invocationId}`,
      width: "100%", height: 0, flexDirection: "column", flexShrink: 0,
      border: ["left"], borderColor: theme.borderSubtle, paddingLeft: 1, marginLeft: 1, visible: false,
    })
    this.onKeyDown = (key) => {
      if (key.name === "return" || key.name === "space") {
        key.preventDefault()
        this.toggle()
      }
    }
    bindSelectableClick(ctx, this.header, () => this.toggle())
    bindSelectableClick(ctx, this.truncationMarker, () => this.openOutput())
    this.add(this.header)
    this.#bodyContainer.add(this.body)
    this.#bodyContainer.add(this.truncationMarker)
    this.add(this.#bodyContainer)
    this.update(source)
  }

  get row(): ToolRow {
    if (this.#row === null) throw new Error("renderable model is released")
    return this.#row
  }

  get expanded(): boolean {
    return !this.#collapsed
  }

  /** Reuse a pooled live card for another invocation. */
  retarget(source: ToolRowSource, expanded?: boolean, onExpansionChange?: (expanded: boolean) => void): void {
    const row = toolRow(source)
    this.blockId = `tool:${row.invocationId}`
    this.id = `tool-${row.invocationId}`
    this.header.id = `${this.blockId}:header`
    this.#bodyContainer.id = `tool-body-${row.invocationId}`
    this.#collapsed = expanded === undefined ? !expandsByDefault(row) : !expanded
    this.#userSetExpansion = expanded !== undefined
    this.#onExpansionChange = onExpansionChange
    this.#selected = false
    this.header.bg = this.#theme.background
    this.#row = row
    this.#source = source
    this.#lastRender = null
    this.update(source)
  }

  /** `rootsGeneration` changes when workspace roots (and so displayed paths) change. */
  update(source: ToolRowSource, availableWidth = this.#availableWidth, rootsGeneration = this.#rootsGeneration): void {
    const previous = this.#row
    this.#availableWidth = Math.max(20, availableWidth)
    this.#rootsGeneration = rootsGeneration
    const last = this.#lastRender
    if (last?.source === source && last.width === this.#availableWidth && last.collapsed === this.#collapsed
      && last.rootsGeneration === rootsGeneration) {
      this.#syncSpinner()
      return
    }
    let row = last?.source === source && previous !== null ? previous : toolRow(source)
    // A live projection releases its arguments when it finishes; keep the
    // summary the row showed while it ran.
    if (previous !== null && previous.invocationId === row.invocationId && previous.args !== null
      && row.live !== null && row.live.args === null) row = { ...row, args: previous.args }
    this.#row = row
    this.#source = source
    if (!this.#userSetExpansion && previous !== null && expandsByDefault(row) && !expandsByDefault(previous)) {
      // Live cards are created while a tool is still running, before its diff
      // exists. Expand on the completion transition, preserving a user choice.
      this.#collapsed = false
    }
    this.#lastRender = { source, width: this.#availableWidth, collapsed: this.#collapsed, rootsGeneration }
    this.#renderHeader()
    this.#syncCommand(row)
    this.#syncDiff(row)
    this.#syncBody(row)
    this.#syncSpinner()
  }

  toggle(): void {
    this.#userSetExpansion = true
    this.#collapsed = !this.#collapsed
    if (this.#source !== null) this.update(this.#source)
    this.#onExpansionChange?.(!this.#collapsed)
  }

  setSelected(selected: boolean): void {
    if (selected === this.#selected) return
    this.#selected = selected
    this.header.bg = selected ? this.#theme.backgroundElement : this.#theme.background
  }

  /** Open the complete output in the output viewer. */
  openOutput(): void {
    const target = this.#row?.output ?? null
    if (target === null) return
    if (target.type === "live") this.#rendering?.onOpenToolOutput?.(target.invocationId)
    else this.#rendering?.onOpenContent?.(target.source)
  }

  override destroy(): void {
    this.#stopSpinner()
    this.#row = null
    this.#source = null
    this.#lastRender = null
    super.destroy()
  }

  #renderHeader(): void {
    const row = this.#row
    if (row === null) return
    const header = toolHeaderContent(row, this.#availableWidth, this.#theme, Date.now(), this.#spinnerFrame)
    const signature = JSON.stringify(header)
    if (signature === this.#headerSignature) return
    this.#headerSignature = signature
    this.header.content = header
  }

  #syncSpinner(): void {
    const running = this.#row?.status === "running" && !this.isDestroyed
    if (!running) { this.#stopSpinner(); return }
    if (this.#spinner !== null) return
    this.#spinner = setInterval(() => {
      if (this.isDestroyed || this.#row?.status !== "running") { this.#stopSpinner(); return }
      this.#spinnerFrame++
      this.#renderHeader()
    }, TOOL_SPINNER_INTERVAL_MS)
    this.#spinner.unref?.()
  }

  #stopSpinner(): void {
    if (this.#spinner === null) return
    clearInterval(this.#spinner)
    this.#spinner = null
  }

  #syncBody(row: ToolRow): void {
    const pending = row.status === "awaiting_approval"
    if (this.#commandContainer !== null) this.#commandContainer.visible = !this.#collapsed && !pending
    if (this.#diffContainer !== null) this.#diffContainer.visible = !this.#collapsed && !pending
    if (this.diff !== null) this.diff.visible = !this.#collapsed && !pending
    if (this.#collapsed) {
      this.body.content = ""
      this.body.height = 0
      this.body.visible = false
      this.truncationMarker.content = ""
      this.truncationMarker.height = 0
      this.truncationMarker.visible = false
      this.#bodyContainer.height = 0
      this.#bodyContainer.visible = false
      this.height = 1
      return
    }
    const preview = toolRowPreview(row)
    const hasBody = preview.content !== ""
    this.body.visible = hasBody
    this.body.content = preview.content
    const bodyContentRows = hasBody ? preview.content.split("\n").length : 0
    this.body.height = bodyContentRows
    const marker = preview.hiddenLines > 0 ? `… ${preview.hiddenLines} more lines · click to open full output`
      : preview.incomplete ? "… click to open full output"
        : row.presentationTitle !== null && row.status === "finished" ? `Open ${row.presentationTitle.toLowerCase()} →` : ""
    this.truncationMarker.content = marker
    this.truncationMarker.height = marker === "" ? 0 : 1
    this.truncationMarker.visible = marker !== ""
    this.#bodyContainer.remove(this.truncationMarker)
    if (preview.markerFirst) this.#bodyContainer.insertBefore(this.truncationMarker, this.body)
    else this.#bodyContainer.add(this.truncationMarker)
    const bodyRows = bodyContentRows + (marker === "" ? 0 : 1)
    this.#bodyContainer.height = bodyRows
    this.#bodyContainer.visible = bodyRows > 0
    const command = this.#commandContainer?.visible === true ? this.#commandContainer.height as number : 0
    const diff = this.#diffContainer?.visible === true ? this.#diffContainer.height as number : 0
    this.height = 1 + bodyRows + command + diff
  }

  #syncCommand(row: ToolRow): void {
    const command = expandedCommand(row)
    const signature = command ?? ""
    if (signature === this.#commandSignature) return
    this.#commandSignature = signature
    if (this.#commandContainer !== null) {
      this.remove(this.#commandContainer)
      this.#commandContainer.destroyRecursively()
      this.#commandContainer = null
      this.command = null
      this.commandPrompt = null
    }
    if (command === null) return
    const content = commandPreview(command)
    const rows = Math.max(1, content.split("\n").length)
    const container = new BoxRenderable(this.ctx, {
      id: `tool-command-row-${row.invocationId}`,
      width: "100%", height: rows, flexDirection: "row", flexShrink: 0, marginLeft: 2,
    })
    this.commandPrompt = new TextRenderable(this.ctx, {
      content: bashPrompt(command), fg: this.#theme.textMuted, width: 2, height: rows, wrapMode: "none",
    })
    this.command = this.#rendering === undefined
      ? new TextRenderable(this.ctx, { content, fg: this.#theme.text, flexGrow: 1, height: rows, wrapMode: "none", selectable: true })
      : new CodeRenderable(this.ctx, {
        id: `tool-command-${row.invocationId}`, flexGrow: 1, height: rows, content, filetype: "bash",
        syntaxStyle: this.#rendering.syntaxStyle,
        ...(this.#rendering.treeSitterClient === undefined ? {} : { treeSitterClient: this.#rendering.treeSitterClient }),
        drawUnstyledText: true, wrapMode: "none", streaming: false, selectable: true,
      })
    container.add(this.commandPrompt)
    container.add(this.command)
    this.#commandContainer = container
    this.insertBefore(container, this.#diffContainer ?? this.#bodyContainer)
  }

  #syncDiff(row: ToolRow): void {
    const proposal = row.diff
    const view = this.#availableWidth < 100 ? "unified" : "split"
    const signature = `${row.diffSource?.sequence ?? ""}\u0000${proposal === null ? "" : `${view}\u0000${proposal.path}\u0000${proposal.unifiedDiff}`}`
    if (signature === this.#diffSignature) return
    this.#diffSignature = signature
    if (this.#diffContainer !== null) {
      this.remove(this.#diffContainer)
      this.#diffContainer.destroyRecursively()
      this.#diffContainer = null
      this.diff = null
    }
    if (proposal === null) {
      if (row.diffSource !== null) {
        const container = new BoxRenderable(this.ctx, { width: "100%", height: 1, flexDirection: "column", flexShrink: 0, marginLeft: 2 })
        this.#appendDiffSource(container, row)
        this.#diffContainer = container
        this.insertBefore(container, this.#bodyContainer)
      }
      return
    }
    const inlineDiff = minimalUnifiedDiff(proposal.path, proposal.unifiedDiff)
    // The transcript remains the sole vertical scroll owner; the inline diff
    // takes its natural height and never traps the wheel in a nested viewport.
    const inlineRows = view === "unified" ? unifiedDiffVisualRows(inlineDiff) : splitDiffVisualRows(inlineDiff)
    const truncated = inlineRows > MAX_INLINE_DIFF_ROWS ? truncateUnifiedDiff(inlineDiff, MAX_INLINE_DIFF_ROWS, view) : null
    const visibleDiff = truncated?.diff ?? inlineDiff
    const filetype = filetypeForPath(proposal.path)
    const rows = view === "unified" ? unifiedDiffVisualRows(visibleDiff) : splitDiffVisualRows(visibleDiff)
    const container = new BoxRenderable(this.ctx, {
      id: `tool-diff-row-${row.invocationId}`,
      width: "100%",
      height: rows + (truncated === null ? 0 : 1) + (row.diffSource === null ? 0 : 1),
      flexDirection: "column",
      flexShrink: 0,
      marginLeft: 2,
    })
    this.diff = this.#rendering === undefined
      ? new TextRenderable(this.ctx, { content: visibleDiff, fg: this.#theme.text, height: rows, wrapMode: "none", selectable: true })
      : new DiffRenderable(this.ctx, {
        id: `tool-diff-${row.invocationId}`,
        width: "100%",
        height: rows,
        diff: visibleDiff,
        ...(filetype === undefined ? {} : { filetype }),
        syntaxStyle: this.#rendering.syntaxStyle,
        ...(this.#rendering.treeSitterClient === undefined ? {} : { treeSitterClient: this.#rendering.treeSitterClient }),
        view,
        syncScroll: false,
        wrapMode: "none",
        showLineNumbers: true,
        addedBg: this.#theme.diffAddedBg,
        removedBg: this.#theme.diffRemovedBg,
        contextBg: this.#theme.backgroundPanel,
      })
    this.diff.selectable = true
    container.add(this.diff)
    if (truncated !== null) {
      container.add(new TextRenderable(this.ctx, {
        content: `… ${truncated.hiddenLines} more lines · Ctrl+R to review`,
        fg: this.#theme.textMuted, height: 1, flexShrink: 0, wrapMode: "none", selectable: true,
      }))
    }
    this.#appendDiffSource(container, row)
    this.#diffContainer = container
    this.insertBefore(container, this.#bodyContainer)
  }

  #appendDiffSource(container: BoxRenderable, row: ToolRow): void {
    const source = row.diffSource
    if (source === null) return
    const link = new TextRenderable(this.ctx, {
      id: `tool-diff-source-${row.invocationId}`, content: "View complete diff", fg: this.#theme.accent,
      height: 1, flexShrink: 0, selectable: true,
    })
    const live = row.live !== null
    bindSelectableClick(this.ctx, link, () => {
      if (live) this.#rendering?.onOpenLiveContent?.(source)
      else this.#rendering?.onOpenContent?.(source)
    })
    container.add(link)
  }
}

function expandsByDefault(row: ToolRow): boolean {
  return row.status === "finished" && !row.isError && row.diff !== null
}

/** The expanded command block is shown only when the header cannot carry the whole command. */
function expandedCommand(row: ToolRow): string | null {
  if (row.status === "awaiting_approval") return null
  const command = row.name === "bash" ? row.display.command ?? (typeof row.args?.command === "string" ? row.args.command : null) : null
  if (command === null) return null
  return command.includes("\n") || truncateToCells(command, HEADER_COMMAND_CELLS) !== command ? command : null
}

/** The expanded body: bounded output, never repeating the diff or approval preview. */
export function toolRowPreview(row: ToolRow): ToolPreview {
  if (row.status === "awaiting_approval") return { content: "Review this action in the approval panel below.", hiddenLines: 0, markerFirst: false, incomplete: false }
  if (row.status !== "finished") {
    if (row.live !== null) return { ...toolOutputPreview(row.live), incomplete: false }
    return { content: [rationaleLine(row.rationale), "Running…"].filter(Boolean).join("\n"), hiddenLines: 0, markerFirst: false, incomplete: false }
  }
  if (row.diff !== null && !row.isError) return { content: rationaleLine(row.rationale), hiddenLines: 0, markerFirst: false, incomplete: false }
  const bounded = boundedToolBody([rationaleLine(row.rationale), row.display.details || "Completed with no output."].filter(Boolean).join("\n"), MAX_PREVIEW_LINES, false)
  return { ...bounded, incomplete: row.display.truncated && row.output !== null }
}

export interface ToolPreview {
  readonly content: string
  readonly hiddenLines: number
  readonly markerFirst: boolean
  readonly incomplete: boolean
}

/** Complete tool body for the output viewer before transcript preview bounding. */
export function toolOutputContent(tool: ToolProjection, live: ToolOutputText | null): string {
  let output: string
  if (tool.status === "finished") {
    output = presentTool(tool).details
    if (tool.display?.truncated) output += "\n… Open full output for the complete result."
  } else {
    if (live === null) throw new Error("live output requires an owned reader")
    output = liveCommand(tool) !== null && tool.chunks.count > 0 ? live.labeled
      : live.plain === "" ? "" : `Live output\n${live.plain}`
  }
  const activity = tool.status === "awaiting_approval" ? "Awaiting approval…"
    : tool.status === "running" ? "Running…"
      : output === "" ? "Completed with no output." : ""
  return [rationaleLine(tool.rationale), output, activity].filter(Boolean).join("\n")
}

/** The mounted live card reads a bounded line window; opening all output materializes the body. */
export function toolOutputPreview(tool: ToolProjection): { readonly content: string; readonly hiddenLines: number; readonly markerFirst: boolean } {
  if (tool.status === "finished") return boundedToolBody(toolOutputContent(tool, null), MAX_PREVIEW_LINES, false)
  const view = tool.chunks.preview()
  const isBash = liveCommand(tool) !== null && tool.chunks.count > 0
  const output = isBash ? view.labeledWindow : view.plainWindow
  const hasOutput = isBash || view.hasOutput
  const prefix = [rationaleLine(tool.rationale), !isBash && hasOutput ? "Live output" : ""].filter(Boolean)
  const lineCount = prefix.length + (hasOutput ? output.lineCount : 0) + 1
  const lines = [...prefix, ...(hasOutput ? output.lines : []), tool.status === "awaiting_approval" ? "Awaiting approval…" : "Running…"]
  if (lineCount <= MAX_PREVIEW_LINES) return { content: lines.join("\n"), hiddenLines: 0, markerFirst: false }
  const retained = lines.slice(-Math.max(1, MAX_PREVIEW_LINES - 1))
  return { content: retained.join("\n"), hiddenLines: lineCount - retained.length, markerFirst: true }
}

function rationaleLine(rationale: string | null): string {
  return rationale === null || rationale.trim() === ""
    ? ""
    : `Why · ${truncateToCells(rationale.replace(/\s+/g, " ").trim(), 160)}`
}

function liveCommand(tool: ToolProjection): string | null {
  if (tool.name !== "bash") return null
  if (tool.status === "finished") return tool.display?.command ?? null
  return typeof tool.args === "object" && tool.args !== null && "command" in tool.args && typeof tool.args.command === "string"
    ? tool.args.command : null
}

function bashPrompt(command: string): string {
  const lines = command.split("\n").length
  const visibleRows = Math.min(COMMAND_PREVIEW_MAX_LINES, lines)
  const prompts: string[] = Array.from({ length: visibleRows }, (_, index) => index === 0 ? "$" : ">")
  if (lines > visibleRows) prompts.push("·")
  return prompts.join("\n")
}

export function boundedToolBody(
  value: string,
  maximum: number,
  retainTail: boolean,
): { readonly content: string; readonly hiddenLines: number; readonly markerFirst: boolean } {
  const lines = value.split("\n")
  if (lines.length <= maximum) return { content: value, hiddenLines: 0, markerFirst: false }
  const retainedRows = Math.max(0, maximum - 1)
  const retained = retainTail ? lines.slice(-retainedRows) : lines.slice(0, retainedRows)
  return { content: retained.join("\n"), hiddenLines: lines.length - retained.length, markerFirst: retainTail }
}
