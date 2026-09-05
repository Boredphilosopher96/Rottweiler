import type { ClientDiagnostics } from "../../client-diagnostics"
import { bindSelectableClick } from "../selectable-click"
import type { HistoryAnchor } from "../../history/controller"
import type { TranscriptContent } from "../../protocol"
import { DISPLAY_TRUNCATION_MARKER } from "../../state/display-buffer"
import type { TranscriptClientState } from "../../recycle-state"
import {
  type BaseRenderable,
  BoxRenderable,
  CodeRenderable,
  DiffRenderable,
  MarkdownRenderable,
  ScrollBoxRenderable,
  TextRenderable,
  bold,
  fg,
  t,
  type RenderContext,
  type SyntaxStyle,
  type TreeSitterClient,
} from "@opentui/core"

import {
  commandPreview,
  commandResultMarkdown,
  COMMAND_PREVIEW_MAX_LINES,
  diffStats,
  displayPath,
  formatCost,
  filetypeForPath,
  getScrollAcceleration,
  minimalUnifiedDiff,
  splitDiffVisualRows,
  unifiedDiffVisualRows,
  presentTool,
  presentableUnifiedDiff,
  terminalMarkdown,
  truncateToCells,
  truncateUnifiedDiff,
  turnMarkdown,
  turnReasoningMarkdown,
} from "../../render"
import type {
  RottweilerState,
  SubagentProjection,
  ToolProjection,
  TranscriptEntry,
} from "../../state"
import type { RottweilerTheme } from "../../theme"

export interface TranscriptRenderableOptions {
  readonly diagnostics?: ClientDiagnostics | undefined
  readonly syntaxStyle: SyntaxStyle
  readonly treeSitterClient?: TreeSitterClient
  readonly onInteraction?: () => void
  readonly onOpenSubagent?: (subagentId: string) => void
  readonly onOpenToolOutput?: (toolCallId: string) => void
  readonly onOpenContent?: (source: import("../../protocol").TranscriptContentSource) => void
  readonly onOpenChild?: (child: Extract<TranscriptContent, { type: "subagent" }>) => void
  readonly onHistoryAnchor?: (anchor: HistoryAnchor) => void
  readonly onHistorySeek?: (ordinal: bigint) => void
  readonly onHistoryAround?: (item: string) => void
  readonly onHistoryBoundary?: (boundary: "first" | "latest") => void
  readonly onHistoryFollowing?: (following: boolean) => void
}

export const GUTTER_BORDER = {
  topLeft: "╎",
  topRight: "╎",
  bottomLeft: "╎",
  bottomRight: "╎",
  horizontal: "╎",
  vertical: "╎",
  topT: "╎",
  bottomT: "╎",
  leftT: "╎",
  rightT: "╎",
  cross: "╎",
} as const

export class ReasoningBlockRenderable extends BoxRenderable {
  readonly header: TextRenderable
  readonly body: TextRenderable
  #blockId: string
  #content = ""
  #expanded = false
  #streaming = false
  #selected = false
  #startedAt: number | null = null
  #elapsedMs: number | null = null
  #width = 80
  readonly #onExpansionChange: (expanded: boolean) => void
  readonly #onInteraction: (() => void) | undefined
  readonly #theme: RottweilerTheme

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    _syntaxStyle: SyntaxStyle,
    options: {
      readonly blockId: string
      readonly content: string
      readonly expanded?: boolean
      readonly streaming?: boolean
      readonly width: number
      readonly treeSitterClient?: TreeSitterClient
      readonly onExpansionChange: (expanded: boolean) => void
      readonly onInteraction?: () => void
    },
  ) {
    super(ctx, {
      id: options.blockId,
      width: "100%",
      flexDirection: "column",
      flexShrink: 0,
      border: ["left"],
      customBorderChars: GUTTER_BORDER,
      borderColor: theme.borderSubtle,
      backgroundColor: theme.background,
      paddingLeft: 1,
      marginTop: 1,
      focusable: false,
    })
    this.#blockId = options.blockId
    this.#theme = theme
    this.#expanded = options.expanded ?? false
    this.#streaming = options.streaming ?? false
    this.#width = options.width
    this.#onExpansionChange = options.onExpansionChange
    this.#onInteraction = options.onInteraction
    this.header = new TextRenderable(ctx, {
      id: `${options.blockId}:header`,
      content: "",
      fg: theme.textMuted,
      bg: theme.background,
      width: "100%",
      height: 1,
      flexShrink: 0,
      wrapMode: "none",
      selectable: true,
    })
    this.body = new TextRenderable(ctx, {
      content: "",
      fg: theme.textMuted,
      bg: theme.background,
      width: "100%",
      wrapMode: "word",
      visible: this.#expanded,
    })
    this.body.selectable = true
    bindSelectableClick(ctx, this.header, () => {
      this.toggle()
      this.#onInteraction?.()
    })
    this.add(this.header)
    this.add(this.body)
    this.update(options.content, this.#streaming, options.width)
  }

  get expanded(): boolean {
    return this.#expanded
  }

  get blockId(): string {
    return this.#blockId
  }

  setBlockId(blockId: string): void {
    if (blockId === this.#blockId) return
    this.#blockId = blockId
    this.id = blockId
    this.header.id = `${blockId}:header`
  }

  setSelected(selected: boolean): void {
    if (selected === this.#selected) return
    this.#selected = selected
    this.header.bg = selected ? this.#theme.backgroundElement : this.#theme.background
  }

  update(content: string, streaming = this.#streaming, width = this.#width): void {
    if (!streaming && this.#streaming && this.#startedAt !== null && this.#elapsedMs === null) {
      this.#elapsedMs = Date.now() - this.#startedAt
    }
    this.#content = presentableReasoning(content)
    if (streaming && this.#content !== "" && this.#startedAt === null) {
      this.#startedAt = Date.now()
    }
    this.#streaming = streaming
    this.#width = width
    this.visible = this.#content !== ""
    this.#layout()
  }

  collapse(notify = true): void {
    if (!this.#expanded) return
    this.#expanded = false
    this.#layout()
    if (notify) this.#onExpansionChange(false)
  }

  expand(notify = true): void {
    if (this.#expanded) return
    this.#expanded = true
    this.#layout()
    if (notify) this.#onExpansionChange(true)
  }

  toggle(): void {
    if (this.#content === "") return
    this.#expanded = !this.#expanded
    this.#layout()
    this.#onExpansionChange(this.#expanded)
  }

  #layout(): void {
    if (this.#content === "") {
      this.header.content = ""
      this.body.visible = false
      return
    }
    const state = this.#streaming
      ? "reasoning"
      : this.#elapsedMs === null
        ? "reasoning"
        : `reasoning · ${formatElapsed(this.#elapsedMs)}`
    const title = this.#expanded ? "" : ` · ${reasoningTitle(this.#content)}`
    const label = `${state}${title}`
    const indicator = this.#expanded ? "⌄" : "›"
    const spacing = " ".repeat(Math.max(1, this.#width - label.length - indicator.length - 3))
    this.header.content = t`${fg(this.#theme.textMuted)(label)}${fg(this.#theme.borderSubtle)(`${spacing}${indicator}`)}`
    this.body.visible = this.#expanded
    this.body.content = this.#expanded ? this.#content : ""
  }
}

export function formatElapsed(elapsedMs: number): string {
  const totalSeconds = Math.max(0, Math.floor(elapsedMs / 1_000))
  if (totalSeconds === 0) return "briefly"
  const hours = Math.floor(totalSeconds / 3_600)
  const minutes = Math.floor((totalSeconds % 3_600) / 60)
  const seconds = totalSeconds % 60
  if (hours > 0) return `${hours}h${minutes.toString().padStart(2, "0")}m${seconds.toString().padStart(2, "0")}s`
  if (minutes > 0) return `${minutes}m${seconds.toString().padStart(2, "0")}s`
  return `${seconds}s`
}

export function presentableReasoning(content: string): string {
  return content
    .replaceAll("[REDACTED]", "")
    .replace(/!\[([^\]]*)]\([^)]*\)/g, "$1")
    .replace(/\[([^\]]+)]\([^)]*\)/g, "$1")
    .replace(/^\s{0,3}#{1,6}\s+/gm, "")
    .replace(/\*\*([^*\n]+)\*\*/g, "$1")
    .replace(/__([^_\n]+)__/g, "$1")
    .replace(/(?<!\*)\*([^*\n]+)\*(?!\*)/g, "$1")
    .replace(/(?<!_)_([^_\n]+)_(?!_)/g, "$1")
    .replace(/~~([^~\n]+)~~/g, "$1")
    .replace(/`([^`\n]+)`/g, "$1")
    .trim()
}

export function reasoningTitle(content: string): string {
  const first = content
    .split("\n")
    .map((line) => line
      .replace(/\[([^\]]+)]\([^)]*\)/g, "$1")
      .replace(/[*_`~]/g, "")
      .replace(/^[\s#>-]+|[\s#>-]+$/g, "")
      .trim())
    .find(Boolean) ?? "Reasoning"
  return truncateToCells(first.replace(/\s+/g, " ").trim(), 72)
}

export class ToolBlockRenderable extends BoxRenderable {
  readonly header: TextRenderable
  readonly body: TextRenderable
  readonly truncationMarker: TextRenderable
  command: CodeRenderable | TextRenderable | null = null
  diff: DiffRenderable | TextRenderable | null = null
  commandPrompt: TextRenderable | null = null
  #commandContainer: BoxRenderable | null = null
  #diffContainer: BoxRenderable | null = null
  readonly #bodyContainer: BoxRenderable
  #commandSignature = ""
  #diffSignature = ""
  #headerSignature = ""
  #collapsed: boolean
  #tool: ToolProjection
  #theme: RottweilerTheme
  #onExpansionChange: ((expanded: boolean) => void) | undefined
  #rendering: TranscriptRenderableOptions | undefined
  #userSetExpansion: boolean
  #selected = false
  #availableWidth: number
  #startedAt = Date.now()
  #rootsGeneration = ""
  #lastRender: { readonly tool: ToolProjection; readonly width: number; readonly collapsed: boolean; readonly elapsed: string; readonly rootsGeneration: string } | null = null
  blockId: string

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    tool: ToolProjection,
    expanded?: boolean,
    onExpansionChange?: (expanded: boolean) => void,
    rendering?: TranscriptRenderableOptions,
  ) {
    const blockId = `tool:${tool.invocationId}`
    super(ctx, {
      id: `tool-${tool.toolCallId}`,
      width: "100%",
      minHeight: 1,
      flexDirection: "column",
      border: false,
      backgroundColor: theme.background,
      // Expansion is mouse-driven while keyboard focus remains owned by the
      // transcript scroller/composer. Individual tool rows must not trap it.
      focusable: false,
      paddingX: 0,
      marginLeft: 2,
      marginTop: 0,
    })
    this.blockId = blockId
    this.#theme = theme
    this.#tool = tool
    this.#availableWidth = Math.max(20, ctx.width)
    const successfulFileEdit =
      tool.status === "finished" && tool.isError !== true && tool.diff !== null
    this.#collapsed = expanded === undefined
      ? tool.status !== "awaiting_approval" && !successfulFileEdit
      : !expanded
    this.#userSetExpansion = expanded !== undefined
    this.#onExpansionChange = onExpansionChange
    this.#rendering = rendering
    this.header = new TextRenderable(ctx, {
      id: `${blockId}:header`,
      content: "",
      fg: theme.text,
      bg: theme.background,
      width: "100%",
      height: 1,
      selectable: true,
    })
    this.body = new TextRenderable(ctx, {
      content: "",
      fg: theme.textMuted,
      wrapMode: "word",
      visible: !this.#collapsed,
      selectable: true,
    })
    this.truncationMarker = new TextRenderable(ctx, {
      content: "",
      fg: theme.textMuted,
      height: 0,
      flexShrink: 0,
      wrapMode: "none",
      visible: false,
      selectable: true,
    })
    this.#bodyContainer = new BoxRenderable(ctx, {
      id: `tool-body-${tool.toolCallId}`,
      width: "100%",
      height: 0,
      flexDirection: "column",
      flexShrink: 0,
      border: ["left"],
      borderColor: theme.borderSubtle,
      paddingLeft: 1,
      visible: !this.#collapsed,
    })
    this.onKeyDown = (key) => {
      if (key.name === "return" || key.name === "space") {
        key.preventDefault()
        this.toggle()
      }
    }
    bindSelectableClick(ctx, this.header, () => this.toggle())
    bindSelectableClick(ctx, this.truncationMarker, () => {
      this.#rendering?.onOpenToolOutput?.(this.#tool.toolCallId)
    })
    this.add(this.header)
    this.#bodyContainer.add(this.body)
    this.#bodyContainer.add(this.truncationMarker)
    this.add(this.#bodyContainer)
    this.update(tool)
  }

  retarget(
    tool: ToolProjection,
    expanded?: boolean,
    onExpansionChange?: (expanded: boolean) => void,
  ): void {
    const blockId = `tool:${tool.invocationId}`
    this.blockId = blockId
    this.id = `tool-${tool.toolCallId}`
    this.header.id = `${blockId}:header`
    this.#bodyContainer.id = `tool-body-${tool.toolCallId}`
    const successfulFileEdit =
      tool.status === "finished" && tool.isError !== true && tool.diff !== null
    this.#collapsed = expanded === undefined
      ? tool.status !== "awaiting_approval" && !successfulFileEdit
      : !expanded
    this.#userSetExpansion = expanded !== undefined
    this.#onExpansionChange = onExpansionChange
    this.#startedAt = Date.now()
    this.#selected = false
    this.header.bg = this.#theme.background
    this.#bodyContainer.visible = !this.#collapsed
    this.#tool = tool
    this.update(tool)
  }

  update(tool: ToolProjection, availableWidth = this.#availableWidth, rootsGeneration = this.#rootsGeneration): void {
    const previousStatus = this.#tool.status
    const previousDiff = this.#tool.diff
    this.#tool = tool
    this.#availableWidth = Math.max(20, availableWidth)
    this.#rootsGeneration = rootsGeneration
    if (tool.status === "awaiting_approval" && previousStatus !== "awaiting_approval") {
      this.#collapsed = false
      this.#bodyContainer.visible = true
    }
    if (
      !this.#userSetExpansion &&
      tool.status === "finished" &&
      tool.isError !== true &&
      tool.diff !== null &&
      (previousStatus !== "finished" || previousDiff === null)
    ) {
      // Live cards are created while a tool is still running, before its diff
      // exists. Expand on the completion transition instead of relying only
      // on constructor state, while preserving an explicit user collapse.
      this.#collapsed = false
      this.#bodyContainer.visible = true
    }
    const elapsed = tool.status === "running" && Date.now() - this.#startedAt >= 3_000
      ? ` · ${formatElapsed(Date.now() - this.#startedAt)}`
      : ""
    const previousRender = this.#lastRender
    if (previousRender?.tool === tool && previousRender.width === this.#availableWidth &&
      previousRender.collapsed === this.#collapsed && previousRender.elapsed === elapsed &&
      previousRender.rootsGeneration === rootsGeneration) return
    this.#lastRender = { tool, width: this.#availableWidth, collapsed: this.#collapsed, elapsed, rootsGeneration }
    this.#syncCommand(tool)
    this.#syncDiff(tool)
    const glyph = tool.status === "awaiting_approval" ? "?" : tool.status === "running" ? "◌" : tool.isError === true ? "✕" : "✓"
    const compact = compactToolPresentation(tool)
    const result =
      tool.status === "finished" && this.#collapsed
        ? compact.summary
        : ""
    const statusColor =
      tool.status === "awaiting_approval"
        ? this.#theme.warning
        : tool.isError === true
          ? this.#theme.error
          : tool.status === "finished"
            ? this.#theme.success
            : this.#theme.info
    const outcome = tool.status === "awaiting_approval"
      ? `${glyph} approval needed`
      : result === ""
        ? `${glyph}${elapsed}`
        : `${glyph} ${result}${elapsed}`
    const rowWidth = Math.max(20, this.#availableWidth - 3)
    const toolName = truncateToCells(tool.name.replaceAll("_", "-"), 12)
    const subjectBudget = Math.max(0, rowWidth - toolName.length - outcome.length - 7)
    const subject = truncateToCells(
      result !== "" && compact.subject !== "" && result.toLowerCase().includes(compact.subject.toLowerCase())
        ? ""
        : compact.subject,
      subjectBudget,
    )
    const name = `${toolName}${subject === "" ? "" : "  "}`
    const prefix = `${this.#collapsed ? "▸" : "⌄"} ${name}${subject}`
    const spacing = " ".repeat(Math.max(2, rowWidth - prefix.length - outcome.length))
    const headerSignature = JSON.stringify([this.#collapsed, name, subject, spacing, outcome])
    if (headerSignature !== this.#headerSignature) {
      this.#headerSignature = headerSignature
      this.header.content = t`${fg(this.#theme.textMuted)(`${this.#collapsed ? "▸" : "⌄"} `)}${fg(this.#theme.secondary)(name)}${subject === "" ? "" : fg(this.#theme.text)(subject)}${spacing}${fg(statusColor)(outcome)}`
    }
    this.header.fg = this.#theme.text
    if (this.#commandContainer !== null) this.#commandContainer.visible = !this.#collapsed
    if (this.#diffContainer !== null) this.#diffContainer.visible = !this.#collapsed
    if (this.diff !== null) this.diff.visible = !this.#collapsed
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
    const preview = toolOutputPreview(tool)
    this.body.visible = true
    this.body.content = preview.content
    const bodyContentRows = Math.max(1, preview.content.split("\n").length)
    this.body.height = bodyContentRows
    this.truncationMarker.content = preview.hiddenLines === 0
      ? ""
      : `… ${preview.hiddenLines} more lines · click to view all`
    this.truncationMarker.height = preview.hiddenLines === 0 ? 0 : 1
    this.truncationMarker.visible = preview.hiddenLines > 0
    this.#bodyContainer.visible = true
    if (preview.markerFirst) {
      this.#bodyContainer.remove(this.truncationMarker)
      this.#bodyContainer.insertBefore(this.truncationMarker, this.body)
    } else {
      this.#bodyContainer.remove(this.truncationMarker)
      this.#bodyContainer.add(this.truncationMarker)
    }
    const bodyRows = bodyContentRows + (preview.hiddenLines === 0 ? 0 : 1)
    this.#bodyContainer.height = bodyRows
    this.height = bodyRows + 1 + (this.#commandContainer?.height ?? 0) + (this.#diffContainer?.height ?? 0)
  }

  #syncCommand(tool: ToolProjection): void {
    const command = bashCommand(tool)
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
    const content = visibleBashCommand(command)
    const rows = Math.max(1, content.split("\n").length)
    const container = new BoxRenderable(this.ctx, {
      id: `tool-command-row-${tool.toolCallId}`,
      width: "100%",
      height: rows,
      flexDirection: "row",
      flexShrink: 0,
    })
    this.commandPrompt = new TextRenderable(this.ctx, {
      content: bashPrompt(command),
      fg: this.#theme.textMuted,
      width: 2,
      height: rows,
      wrapMode: "none",
    })
    this.command = this.#rendering === undefined
      ? new TextRenderable(this.ctx, {
        content,
        fg: this.#theme.text,
        flexGrow: 1,
        height: rows,
        wrapMode: "none",
        selectable: true,
      })
      : new CodeRenderable(this.ctx, {
        id: `tool-command-${tool.toolCallId}`,
        flexGrow: 1,
        height: rows,
        content,
        filetype: "bash",
        syntaxStyle: this.#rendering.syntaxStyle,
        ...(this.#rendering.treeSitterClient === undefined
          ? {}
          : { treeSitterClient: this.#rendering.treeSitterClient }),
        drawUnstyledText: true,
        wrapMode: "none",
        streaming: false,
        selectable: true,
      })
    container.add(this.commandPrompt)
    container.add(this.command)
    this.#commandContainer = container
    this.insertBefore(container, this.#diffContainer ?? this.#bodyContainer)
  }

  #syncDiff(tool: ToolProjection): void {
    const proposal = readToolDiff(tool)
    const view = this.#availableWidth < 100 ? "unified" : "split"
    const signature = proposal === null ? "" : `${view}\u0000${proposal.path}\u0000${proposal.unifiedDiff}`
    if (signature === this.#diffSignature) return
    this.#diffSignature = signature
    if (this.#diffContainer !== null) {
      this.remove(this.#diffContainer)
      this.#diffContainer.destroyRecursively()
      this.#diffContainer = null
      this.diff = null
    }
    if (proposal === null) return
    const inlineDiff = minimalUnifiedDiff(proposal.path, proposal.unifiedDiff)
    // DiffRenderable itself resizes in place, but crossing the view threshold
    // also changes the pre-truncated diff and its surrounding marker rows. Keep
    // that exceptional rebuild local to the diff subregion instead of replacing
    // the ToolBlockRenderable or its containing turn card.
    // The diff is changed-lines-only, so give it its natural height. The
    // transcript remains the sole vertical scroll owner; the inline diff never
    // traps the wheel in a nested viewport.
    const inlineRows = view === "unified" ? unifiedDiffVisualRows(inlineDiff) : splitDiffVisualRows(inlineDiff)
    const truncated = inlineRows > 24
      ? truncateUnifiedDiff(inlineDiff, 24, view)
      : null
    const visibleDiff = truncated?.diff ?? inlineDiff
    const filetype = filetypeForPath(proposal.path)
    const stats = diffStats(inlineDiff)
    const rows = view === "unified"
      ? unifiedDiffVisualRows(visibleDiff)
      : splitDiffVisualRows(visibleDiff)
    const container = new BoxRenderable(this.ctx, {
      id: `tool-diff-row-${tool.toolCallId}`,
      width: "100%",
      height: rows + 1 + (truncated === null ? 0 : 1),
      flexDirection: "column",
      flexShrink: 0,
    })
    container.add(new TextRenderable(this.ctx, {
      content: `${displayPath(proposal.path)} · +${stats.added} −${stats.removed}`,
      fg: this.#theme.textMuted,
      height: 1,
      flexShrink: 0,
      wrapMode: "none",
      selectable: true,
    }))
    this.diff = this.#rendering === undefined
      ? new TextRenderable(this.ctx, {
        content: visibleDiff,
        fg: this.#theme.text,
        height: rows,
        wrapMode: "none",
        selectable: true,
      })
      : new DiffRenderable(this.ctx, {
        id: `tool-diff-${tool.toolCallId}`,
        width: "100%",
        height: rows,
        diff: visibleDiff,
        ...(filetype === undefined ? {} : { filetype }),
        syntaxStyle: this.#rendering.syntaxStyle,
        ...(this.#rendering.treeSitterClient === undefined
          ? {}
          : { treeSitterClient: this.#rendering.treeSitterClient }),
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
        fg: this.#theme.textMuted,
        height: 1,
        flexShrink: 0,
        wrapMode: "none",
        selectable: true,
      }))
    }
    this.#diffContainer = container
    this.insertBefore(container, this.#bodyContainer)
  }

  get expanded(): boolean {
    return !this.#collapsed
  }

  toggle(): void {
    this.#userSetExpansion = true
    this.#collapsed = !this.#collapsed
    this.#bodyContainer.visible = !this.#collapsed
    this.update(this.#tool)
    this.#onExpansionChange?.(!this.#collapsed)
  }

  setSelected(selected: boolean): void {
    if (selected === this.#selected) return
    this.#selected = selected
    this.header.bg = selected ? this.#theme.backgroundElement : this.#theme.background
  }
}

/** Complete tool-card body content before compact transcript preview bounding. */
export function toolOutputContent(tool: ToolProjection): string {
  let output: string
  if (tool.status === "finished" || (bashCommand(tool) !== null && tool.chunks.count > 0)) {
    output = presentTool(tool).details
  } else {
    const live = tool.chunks.read().plain
    output = live === "" ? "" : `Live output\n${live}`
  }
  const activity = tool.status === "awaiting_approval"
    ? "Awaiting approval…"
    : tool.status === "running"
      ? "Running…"
      : output === ""
        ? "Completed with no output."
        : ""
  const rationale = toolRationale(tool)
  return [rationale, output, activity]
    .filter(Boolean)
    .join("\n")
}

/** The mounted live card reads a bounded line window; opening all output materializes the body. */
export function toolOutputPreview(tool: ToolProjection): { readonly content: string; readonly hiddenLines: number; readonly markerFirst: boolean } {
  const maximum = 8
  if (tool.status !== "running") return boundedToolBody(toolOutputContent(tool), maximum, false)
  const view = tool.chunks.read()
  const isBash = bashCommand(tool) !== null && tool.chunks.count > 0
  const output = isBash ? view.labeledWindow : view.plainWindow
  const hasOutput = isBash || view.plain !== ""
  const rationale = toolRationale(tool)
  const prefix = [rationale, !isBash && hasOutput ? "Live output" : ""].filter(Boolean)
  const lineCount = prefix.length + (hasOutput ? output.lineCount : 0) + 1
  const lines = [...prefix, ...(hasOutput ? output.lines : []), "Running…"]
  if (lineCount <= maximum) return { content: lines.join("\n"), hiddenLines: 0, markerFirst: false }
  const retained = lines.slice(-Math.max(1, maximum - 1))
  return { content: retained.join("\n"), hiddenLines: lineCount - retained.length, markerFirst: true }
}

export function toolRationale(tool: ToolProjection): string {
  return tool.rationale === null || tool.rationale.trim() === ""
    ? ""
    : `Why · ${truncateToCells(tool.rationale.replace(/\s+/g, " ").trim(), 160)}`
}

export function bashCommand(tool: ToolProjection): string | null {
  if ((tool.name !== "bash" && tool.name !== "shell") || !isRecord(tool.args)) return null
  return typeof tool.args.command === "string" ? tool.args.command : null
}

export function visibleBashCommand(command: string): string {
  return commandPreview(command)
}

export function bashPrompt(command: string): string {
  const visibleRows = Math.min(COMMAND_PREVIEW_MAX_LINES, command.split("\n").length)
  const prompts: string[] = Array.from({ length: visibleRows }, (_, index) => index === 0 ? "$" : ">")
  if (command.split("\n").length > visibleRows) prompts.push("·")
  return prompts.join("\n")
}

export function boundedToolBody(
  value: string,
  maximum: number,
  retainTail: boolean,
): { readonly content: string; readonly hiddenLines: number; readonly markerFirst: boolean } {
  const lines = value.split("\n")
  if (lines.length <= maximum) {
    return { content: value, hiddenLines: 0, markerFirst: false }
  }
  const retainedRows = Math.max(0, maximum - 1)
  const retained = retainTail ? lines.slice(-retainedRows) : lines.slice(0, retainedRows)
  return {
    content: retained.join("\n"),
    hiddenLines: lines.length - retained.length,
    markerFirst: retainTail,
  }
}

export function readToolDiff(tool: ToolProjection): { path: string; unifiedDiff: string } | null {
  if (!isRecord(tool.diff)) return null
  return typeof tool.diff.path === "string" && typeof tool.diff.unified_diff === "string"
    ? {
      path: tool.diff.path,
      unifiedDiff: presentableUnifiedDiff(tool.diff.path, tool.diff.unified_diff),
    }
    : null
}

export function compactToolPresentation(tool: ToolProjection): { subject: string; summary: string } {
  const presentation = presentTool(tool)
  const arguments_ = isRecord(tool.args) ? tool.args : null
  const fallbackSubject = [
    arguments_?.command,
    arguments_?.path,
    arguments_?.file_path,
    arguments_?.pattern,
    arguments_?.query,
  ].find((value): value is string => typeof value === "string" && value.trim() !== "") ?? ""
  const subject = truncateToCells(
    (presentation.subject || fallbackSubject).replace(/\s+/g, " ").trim(),
    80,
  )
  const summary = truncateToCells(presentation.summary.replace(/\s+/g, " ").trim(), 56)
  return { subject, summary }
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}

export class SubagentPanelRenderable extends BoxRenderable {
  readonly header: TextRenderable
  readonly rows = new Map<string, TextRenderable>()
  readonly #theme: RottweilerTheme
  readonly #onOpenSubagent: ((subagentId: string) => void) | undefined
  #rowOrder: readonly string[] = []

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    onOpenSubagent?: (subagentId: string) => void,
  ) {
    super(ctx, {
      id: "subagent-progress",
      width: "100%",
      height: 0,
      flexDirection: "column",
      flexShrink: 0,
      border: ["left"],
      borderStyle: "single",
      borderColor: theme.info,
      backgroundColor: theme.background,
      paddingLeft: 1,
      marginTop: 1,
      visible: false,
    })
    this.#theme = theme
    this.#onOpenSubagent = onOpenSubagent
    this.header = new TextRenderable(ctx, {
      content: "",
      fg: theme.info,
      height: 1,
      flexShrink: 0,
    })
    this.add(this.header)
  }

  update(subagents: readonly SubagentProjection[], total = subagents.length): void {
    const nextOrder = subagents.map((subagent) => subagent.projectionId)
    if (
      nextOrder.length !== this.#rowOrder.length ||
      nextOrder.some((subagentId, index) => subagentId !== this.#rowOrder[index])
    ) {
      for (const row of this.rows.values()) {
        this.remove(row)
        row.destroyRecursively()
      }
      this.rows.clear()
      this.#rowOrder = nextOrder
    }
    const currentIds = new Set(subagents.map((subagent) => subagent.projectionId))
    for (const [subagentId, row] of this.rows) {
      if (!currentIds.has(subagentId)) {
        this.remove(row)
        row.destroyRecursively()
        this.rows.delete(subagentId)
      }
    }

    const running = subagents.filter((subagent) => subagent.status === "running").length
    this.header.content = `AGENTS · ${running} running · ${total} total`
    for (const [index, subagent] of subagents.entries()) {
      let row = this.rows.get(subagent.projectionId)
      if (row === undefined) {
        row = new TextRenderable(this.ctx, {
          content: "",
          fg: this.#theme.textMuted,
          height: 1,
          flexShrink: 0,
        })
        this.rows.set(subagent.projectionId, row)
        this.add(row)
      }
      const glyph = subagentGlyph(subagent.status)
      const branch = index === subagents.length - 1 ? "└─" : "├─"
      const detail = subagentDetail(subagent)
      const task = truncateToCells(subagent.task.replace(/\s+/g, " ").trim(), 72)
      row.content = `${branch} ${glyph} ${task}${detail === "" ? "" : ` · ${detail}`}`
      row.onMouseDown = () => this.#onOpenSubagent?.(subagent.subagentId)
      row.fg =
        subagent.status === "failed"
          ? this.#theme.error
          : subagent.status === "completed"
            ? this.#theme.success
            : subagent.status === "cancelled" ||
              subagent.status === "timed_out" ||
              subagent.status === "max_turns"
              ? this.#theme.warning
              : this.#theme.info
    }
    this.visible = subagents.length > 0
    this.height = subagents.length === 0 ? 0 : subagents.length + 1
  }
}

export function subagentDetail(subagent: SubagentProjection): string {
  if (subagent.status === "running") {
    return subagent.activity ?? "starting"
  }
  const files = subagent.touchedFileCount === 0 ? "" : ` · ${subagent.touchedFileCount} files`
  const diff = subagent.diffArtifactId === null ? "" : " · diff ready"
  const summary = subagent.summary === null
    ? subagent.status.replaceAll("_", " ")
    : truncateToCells(subagent.summary.replace(/\s+/g, " ").trim(), 72)
  return `${summary}${files}${diff}`
}

export function subagentGlyph(status: SubagentProjection["status"]): string {
  switch (status) {
    case "running":
      return "◌"
    case "completed":
      return "✓"
    case "failed":
      return "✕"
    case "cancelled":
      return "■"
    case "timed_out":
      return "◷"
    case "max_turns":
      return "◇"
  }
}
