import { DISPLAY_TRUNCATION_MARKER } from "../state/display-buffer"
import type { TranscriptClientState } from "../recycle-state"
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
} from "../render"
import type {
  RottweilerState,
  SubagentProjection,
  ToolProjection,
  TranscriptEntry,
} from "../state"
import type { RottweilerTheme } from "../theme"

export interface TranscriptRenderableOptions {
  readonly syntaxStyle: SyntaxStyle
  readonly treeSitterClient?: TreeSitterClient
  readonly onInteraction?: () => void
  readonly onOpenSubagent?: (subagentId: string) => void
  readonly onOpenToolOutput?: (toolCallId: string) => void
}

const MAX_VISIBLE_SUBAGENTS = 8
// OpenTUI viewport culling skips paint work but retains every mounted renderable.
// Bound the expensive live card tree independently of the reducer's retained
// recent history. Long sessions otherwise grow by roughly one pair of Markdown
// renderables per turn even after context compaction.
const MAX_MOUNTED_TRANSCRIPT_ENTRIES = 16

const GUTTER_BORDER = {
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

const USER_GUTTER_BORDER = {
  ...GUTTER_BORDER,
  topLeft: "▌",
  bottomLeft: "▌",
  vertical: "▌",
} as const

function entryKey(entry: TranscriptEntry): string {
  return `${entry.sequenceId}:${entry.agentTurn}:${entry.turn.role}`
}

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
    this.header.onMouseDown = () => {
      this.toggle()
      this.#onInteraction?.()
    }
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

function presentableReasoning(content: string): string {
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

function reasoningTitle(content: string): string {
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
  blockId: string

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    tool: ToolProjection,
    expanded?: boolean,
    onExpansionChange?: (expanded: boolean) => void,
    rendering?: TranscriptRenderableOptions,
  ) {
    const blockId = `tool:${tool.toolCallId}`
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
    // Toggle only after a deliberate click. Mouse-down must remain available
    // to start a text selection, and a completed header drag must not reflow
    // the card underneath the selection gesture.
    this.header.onMouseUp = () => {
      const selection = this.ctx.getSelection()
      if (selection === null || selection.getSelectedText().trim() === "") this.toggle()
    }
    this.truncationMarker.onMouseUp = () => {
      const selection = this.ctx.getSelection()
      if (selection === null || selection.getSelectedText().trim() === "") {
        this.#rendering?.onOpenToolOutput?.(this.#tool.toolCallId)
      }
    }
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
    const blockId = `tool:${tool.toolCallId}`
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

  update(tool: ToolProjection, availableWidth = this.#availableWidth): void {
    const previousStatus = this.#tool.status
    const previousDiff = this.#tool.diff
    this.#tool = tool
    this.#availableWidth = Math.max(20, availableWidth)
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
    this.#syncCommand(tool)
    this.#syncDiff(tool)
    const glyph = tool.status === "awaiting_approval" ? "?" : tool.status === "running" ? "◌" : tool.isError === true ? "✕" : "✓"
    const compact = compactToolPresentation(tool)
    const result =
      tool.status === "finished" && this.#collapsed
        ? compact.summary
        : ""
    const elapsed = tool.status === "running" && Date.now() - this.#startedAt >= 3_000
      ? ` · ${formatElapsed(Date.now() - this.#startedAt)}`
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

function toolRationale(tool: ToolProjection): string {
  return tool.rationale === null || tool.rationale.trim() === ""
    ? ""
    : `Why · ${truncateToCells(tool.rationale.replace(/\s+/g, " ").trim(), 160)}`
}

function bashCommand(tool: ToolProjection): string | null {
  if ((tool.name !== "bash" && tool.name !== "shell") || !isRecord(tool.args)) return null
  return typeof tool.args.command === "string" ? tool.args.command : null
}

function visibleBashCommand(command: string): string {
  return commandPreview(command)
}

function bashPrompt(command: string): string {
  const visibleRows = Math.min(COMMAND_PREVIEW_MAX_LINES, command.split("\n").length)
  const prompts: string[] = Array.from({ length: visibleRows }, (_, index) => index === 0 ? "$" : ">")
  if (command.split("\n").length > visibleRows) prompts.push("·")
  return prompts.join("\n")
}

function boundedToolBody(
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

function readToolDiff(tool: ToolProjection): { path: string; unifiedDiff: string } | null {
  if (!isRecord(tool.diff)) return null
  return typeof tool.diff.path === "string" && typeof tool.diff.unified_diff === "string"
    ? {
        path: tool.diff.path,
        unifiedDiff: presentableUnifiedDiff(tool.diff.path, tool.diff.unified_diff),
      }
    : null
}

function toolBlockExpanded(
  tool: ToolProjection,
  expansion?: ReadonlyMap<string, boolean>,
): boolean {
  return expansion?.get(tool.toolCallId) ?? tool.status === "awaiting_approval"
}

function compactToolPresentation(tool: ToolProjection): { subject: string; summary: string } {
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

export function toolDisplayName(name: string): string {
  return ({
    read: "Read file",
    write: "Write file",
    edit: "Edit file",
    multi_edit: "Edit files",
    grep: "Search text",
    search: "Search text",
    glob: "Find files",
    ls: "List directory",
    bash: "Terminal command",
    shell: "Terminal command",
    background_status: "Check background process",
    background_output: "Read background output",
    background_kill: "Stop background process",
    webfetch: "Fetch URL",
    websearch: "Search web",
    todo: "Update todos",
    ask_user: "Ask user",
    submit_plan: "Submit plan",
    symbols: "Find symbols",
    apply_worktree_diff: "Apply changes",
    tool_search: "Find tools",
    mcp_call: "MCP tool",
    spawn_agent: "Start child agent",
  } as Record<string, string>)[name] ?? name
    .replace(/^mcp__/, "MCP · ")
    .replaceAll("__", " · ")
    .replaceAll("_", " ")
    .replace(/^./, (letter) => letter.toUpperCase())
}

function isRecord(value: unknown): value is Record<string, unknown> {
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

function subagentDetail(subagent: SubagentProjection): string {
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

interface TurnCardViewModel {
  readonly key: string
  readonly first: boolean
  readonly width: number
  readonly entry: TranscriptEntry
  readonly detail: string | null
  readonly tools: readonly ToolProjection[]
  readonly visibleSubagents: readonly SubagentProjection[]
  readonly subagentTotal: number
  readonly toolExpansion: readonly (boolean | undefined)[]
  readonly reasoningExpanded: boolean
  readonly rootsGeneration: string
}

function reuseReferenceArray<T>(
  previous: readonly T[] | undefined,
  next: readonly T[],
): readonly T[] {
  if (
    previous !== undefined &&
    previous.length === next.length &&
    next.every((value, index) => value === previous[index])
  ) return previous
  return next
}

function sameTurnCardViewModel(
  previous: TurnCardViewModel,
  next: TurnCardViewModel,
): boolean {
  return previous.key === next.key &&
    previous.first === next.first &&
    previous.width === next.width &&
    previous.entry === next.entry &&
    previous.detail === next.detail &&
    previous.tools === next.tools &&
    previous.visibleSubagents === next.visibleSubagents &&
    previous.subagentTotal === next.subagentTotal &&
    previous.toolExpansion === next.toolExpansion &&
    previous.reasoningExpanded === next.reasoningExpanded &&
    previous.rootsGeneration === next.rootsGeneration
}

class TurnCardRenderable extends BoxRenderable {
  readonly header: TextRenderable
  markdown!: MarkdownRenderable
  reasoning: ReasoningBlockRenderable | null = null
  shellCommand: CodeRenderable | TextRenderable | null = null
  shellOutput: TextRenderable | null = null
  readonly #theme: RottweilerTheme
  readonly #syntaxStyle: SyntaxStyle
  readonly #treeSitterClient: TreeSitterClient | undefined
  readonly #onToolExpansionChange: (toolCallId: string, expanded: boolean) => void
  readonly #onReasoningExpansionChange: (expanded: boolean) => void
  readonly #onInteraction: (() => void) | undefined
  readonly #onOpenSubagent: ((subagentId: string) => void) | undefined
  readonly #onOpenToolOutput: ((toolCallId: string) => void) | undefined
  readonly #toolCards = new Map<string, ToolBlockRenderable>()
  #toolOrder: readonly string[] = []
  #subagentPanel: SubagentPanelRenderable | null = null
  #entryRenderables: BaseRenderable[] = []
  #viewModel: TurnCardViewModel

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    syntaxStyle: SyntaxStyle,
    viewModel: TurnCardViewModel,
    onToolExpansionChange: (toolCallId: string, expanded: boolean) => void,
    onReasoningExpansionChange: (expanded: boolean) => void,
    onInteraction: (() => void) | undefined,
    onOpenSubagent: ((subagentId: string) => void) | undefined,
    onOpenToolOutput: ((toolCallId: string) => void) | undefined,
    treeSitterClient?: TreeSitterClient,
  ) {
    const shell = viewModel.entry.presentation === "shell_result"
      ? viewModel.entry.shell
      : undefined
    const markdown = turnCardMarkdown(viewModel.entry, viewModel.width)
    const toolOnly = viewModel.entry.turn.role === "tool" && markdown === ""
    super(ctx, {
      id: `turn-${viewModel.key}`,
      width: viewModel.width,
      flexDirection: "column",
      flexShrink: 0,
      border: shell !== undefined
        ? true
        : viewModel.entry.turn.role === "user"
          ? ["left"]
          : false,
      ...(shell === undefined
        ? viewModel.entry.turn.role === "user"
          ? {
              customBorderChars: USER_GUTTER_BORDER,
              borderColor: theme.primary,
            }
          : {}
        : {
            borderStyle: "single" as const,
            borderColor: shell.active
              ? theme.info
              : shell.status === 0
                ? theme.success
              : shell.status === null
                ? theme.textMuted
                : theme.error,
          }),
      backgroundColor: shell !== undefined ? theme.backgroundPanel : theme.background,
      paddingX: viewModel.entry.turn.role === "user" || shell !== undefined ? 1 : 0,
      paddingY: 0,
      marginTop: viewModel.first || toolOnly ? 0 : 1,
    })
    this.#theme = theme
    this.#syntaxStyle = syntaxStyle
    this.#treeSitterClient = treeSitterClient
    this.#viewModel = viewModel
    this.#onToolExpansionChange = onToolExpansionChange
    this.#onReasoningExpansionChange = onReasoningExpansionChange
    this.#onInteraction = onInteraction
    this.#onOpenSubagent = onOpenSubagent
    this.#onOpenToolOutput = onOpenToolOutput
    this.header = new TextRenderable(ctx, {
      content: "",
      fg: theme.info,
      height: 1,
      flexShrink: 0,
      visible: false,
      selectable: true,
    })
    // Selection can focus a retained transcript node. The app restores its
    // configured keyboard-input target after the pointer interaction ends.
    this.onMouseUp = () => this.#onInteraction?.()
    this.#mountEntryRegion(viewModel)
    this.#reconcileTools(viewModel, undefined)
    this.#reconcileSubagents(viewModel)
  }

  get viewModel(): TurnCardViewModel {
    return this.#viewModel
  }

  canRecycleFor(viewModel: TurnCardViewModel): boolean {
    return recyclablePlainEntry(this.#viewModel) && recyclablePlainEntry(viewModel)
  }

  update(viewModel: TurnCardViewModel, toolPool?: ToolBlockRenderable[]): void {
    const previous = this.#viewModel
    if (previous === viewModel) return
    const entryChanged = previous.entry !== viewModel.entry
    const canRecycleEntry = recyclablePlainEntry(previous) && recyclablePlainEntry(viewModel)
    const widthChanged = previous.width !== viewModel.width
    const rootsChanged = previous.rootsGeneration !== viewModel.rootsGeneration
    this.#viewModel = viewModel

    if (widthChanged) {
      this.width = viewModel.width
      this.markdown.width = Math.max(1, viewModel.width - 2)
      if (this.reasoning !== null) {
        this.reasoning.update(
          turnReasoningMarkdown(viewModel.entry.turn),
          false,
          viewModel.width,
        )
      }
    }
    if (entryChanged) {
      if (canRecycleEntry) {
        // A bounded transcript window must recycle its plain cards. Destroying
        // and recreating one MarkdownRenderable per completed turn leaves
        // native renderer allocations resident even after the JS objects are
        // collected, which defeats the window's RSS bound.
        this.#applyCardStyle(viewModel.entry, false)
        this.#updateHeader(viewModel)
        this.markdown.marginLeft = viewModel.entry.turn.role === "assistant" ? 2 : 0
        this.markdown.width = Math.max(1, viewModel.width - 2)
        this.markdown.content = turnCardMarkdown(viewModel.entry, viewModel.width)
        this.markdown.visible = true
      } else {
        // Committed entries are normally immutable. Structured replacements
        // rebuild only their content region; keyed tool cards and the subagent
        // panel remain mounted.
        this.#clearEntryRegion()
        this.#mountEntryRegion(viewModel)
      }
    } else {
      if (rootsChanged) this.markdown.content = turnCardMarkdown(viewModel.entry, viewModel.width)
      if (previous.detail !== viewModel.detail) this.#updateHeader(viewModel)
    }
    this.#reconcileTools(viewModel, previous, toolPool)
    if (
      previous.visibleSubagents !== viewModel.visibleSubagents ||
      previous.subagentTotal !== viewModel.subagentTotal
    ) this.#reconcileSubagents(viewModel)
  }

  #clearEntryRegion(): void {
    if (this.header.parent === this) this.remove(this.header)
    for (const renderable of this.#entryRenderables) {
      if (renderable.parent === this) this.remove(renderable)
      renderable.destroyRecursively()
    }
    this.#entryRenderables = []
    this.reasoning = null
    this.shellCommand = null
    this.shellOutput = null
  }

  #mountEntryRegion(viewModel: TurnCardViewModel): void {
    const entry = viewModel.entry
    const shell = entry.presentation === "shell_result" ? entry.shell : undefined
    const markdownContent = turnCardMarkdown(entry, viewModel.width)
    const reasoningContent = shell === undefined ? turnReasoningMarkdown(entry.turn) : ""
    const toolOnly = entry.turn.role === "tool" && markdownContent === ""
    this.#applyCardStyle(entry, toolOnly)
    this.#updateHeader(viewModel)
    this.markdown = new MarkdownRenderable(this.ctx, {
      id: `markdown-${viewModel.key}`,
      content: markdownContent,
      syntaxStyle: this.#syntaxStyle,
      ...(this.#treeSitterClient === undefined ? {} : { treeSitterClient: this.#treeSitterClient }),
      fg: this.#theme.markdownText,
      conceal: true,
      concealCode: false,
      streaming: false,
      width: Math.max(1, viewModel.width - (entry.turn.role === "assistant" ? 2 : 2)),
      marginLeft: entry.turn.role === "assistant" ? 2 : 0,
      flexShrink: 0,
      visible: !toolOnly,
      internalBlockMode: "top-level",
      tableOptions: { style: "grid", widthMode: "full", wrapMode: "word" },
    })
    this.markdown.selectable = true
    this.#entryRenderables.push(this.markdown)
    this.reasoning = reasoningContent === ""
      ? null
      : new ReasoningBlockRenderable(this.ctx, this.#theme, this.#syntaxStyle, {
          blockId: `reasoning:${viewModel.key}`,
          content: reasoningContent,
          expanded: viewModel.reasoningExpanded,
          streaming: false,
          width: viewModel.width,
          ...(this.#treeSitterClient === undefined
            ? {}
            : { treeSitterClient: this.#treeSitterClient }),
          onExpansionChange: this.#onReasoningExpansionChange,
          ...(this.#onInteraction === undefined ? {} : { onInteraction: this.#onInteraction }),
        })
    if (this.reasoning !== null) this.#entryRenderables.push(this.reasoning)

    if (shell !== undefined) {
      this.#insertBeforeProjections(this.header)
      const content = visibleBashCommand(shell.command)
      const rows = Math.max(1, content.split("\n").length)
      const commandRow = new BoxRenderable(this.ctx, {
        id: `shell-command-row-${shell.shellId}`,
        width: "100%",
        height: rows,
        flexDirection: "row",
        flexShrink: 0,
        marginTop: 1,
      })
      commandRow.add(new TextRenderable(this.ctx, {
        content: bashPrompt(shell.command),
        fg: this.#theme.textMuted,
        width: 2,
        height: rows,
        wrapMode: "none",
      }))
      const renderedCommand = this.#treeSitterClient === undefined
        ? new TextRenderable(this.ctx, {
            content,
            fg: this.#theme.text,
            flexGrow: 1,
            height: rows,
            wrapMode: "none",
            selectable: true,
          })
        : new CodeRenderable(this.ctx, {
            id: `shell-command-${shell.shellId}`,
            flexGrow: 1,
            height: rows,
            content,
            filetype: "bash",
            syntaxStyle: this.#syntaxStyle,
            treeSitterClient: this.#treeSitterClient,
            drawUnstyledText: true,
            wrapMode: "none",
            streaming: false,
            selectable: true,
          })
      this.shellCommand = renderedCommand
      commandRow.add(renderedCommand)
      this.#entryRenderables.push(commandRow)
      this.#insertBeforeProjections(commandRow)
      const output = shell.capturedOutput.trimEnd()
      const renderedOutput = new TextRenderable(this.ctx, {
        id: `shell-output-${shell.shellId}`,
        content: output === ""
          ? shell.active ? "Running in the foreground terminal…" : "Completed with no output."
          : `Output${shell.outputTruncated ? " · truncated" : ""}\n${output}`,
        fg: output === "" ? this.#theme.textMuted : this.#theme.text,
        wrapMode: "word",
        flexShrink: 0,
        marginTop: 1,
        selectable: true,
      })
      this.shellOutput = renderedOutput
      this.#entryRenderables.push(renderedOutput)
      this.#insertBeforeProjections(renderedOutput)
      return
    }
    if (!toolOnly) {
      this.#insertBeforeProjections(this.header)
      if (this.reasoning !== null) this.#insertBeforeProjections(this.reasoning)
      this.#insertBeforeProjections(this.markdown)
    }
  }

  #applyCardStyle(entry: TranscriptEntry, toolOnly: boolean): void {
    const shell = entry.presentation === "shell_result" ? entry.shell : undefined
    this.border = shell !== undefined
      ? true
      : entry.turn.role === "user"
        ? ["left"]
        : false
    if (shell !== undefined) {
      this.borderStyle = "single"
      this.borderColor = shell.active
        ? this.#theme.info
        : shell.status === 0
          ? this.#theme.success
        : shell.status === null
          ? this.#theme.textMuted
          : this.#theme.error
    }
    if (shell === undefined && entry.turn.role === "user") {
      this.customBorderChars = USER_GUTTER_BORDER
      this.borderColor = this.#theme.primary
    }
    this.backgroundColor = shell !== undefined ? this.#theme.backgroundPanel : this.#theme.background
    this.paddingX = entry.turn.role === "user" || shell !== undefined ? 1 : 0
    this.paddingY = 0
    this.marginTop = this.#viewModel.first || toolOnly ? 0 : 1
  }

  #updateHeader(viewModel: TurnCardViewModel): void {
    const entry = viewModel.entry
    const shell = entry.presentation === "shell_result" ? entry.shell : undefined
    const markdown = turnCardMarkdown(entry, viewModel.width)
    const toolOnly = entry.turn.role === "tool" && markdown === ""
    const role = shell !== undefined
      ? "Shell"
      : entry.presentation === "command_result"
      ? "Command result"
      : entry.turn.role === "assistant"
        ? "Rottweiler"
        : entry.turn.role === "user"
          ? "You"
          : "Tools"
    const marker = entry.turn.role === "assistant"
      ? "● "
      : entry.turn.role === "user"
        ? ""
        : entry.turn.role === "tool"
          ? "▸ "
          : ""
    const label = role.toLowerCase()
    this.header.content = shell === undefined
      ? entry.turn.role === "assistant"
        ? t`${fg(this.#theme.accent)(marker)}${bold(fg(this.#theme.text)(label))}${viewModel.detail === null ? "" : fg(this.#theme.textMuted)(`  ${viewModel.detail}`)}`
        : entry.turn.role === "user"
          ? t`${bold(fg(this.#theme.primary)(label))}${viewModel.detail === null ? "" : fg(this.#theme.textMuted)(`  ${viewModel.detail}`)}`
        : entry.presentation === "command_result"
          ? `${marker}${label} · ${entry.title ?? "completed"}`
          : `${marker}${label}${viewModel.detail === null ? "" : ` · ${viewModel.detail}`}`
      : shellHeader(shell.active, shell.status)
    this.header.fg = shell === undefined
      ? entry.turn.role === "assistant"
        ? this.#theme.text
        : entry.turn.role === "user"
          ? this.#theme.primary
          : this.#theme.info
      : shell.active
        ? this.#theme.info
        : shell.status === 0
          ? this.#theme.success
          : shell.status === null
            ? this.#theme.textMuted
            : this.#theme.error
    this.header.height = toolOnly ? 0 : 1
    this.header.visible = shell !== undefined || (!toolOnly && markdown !== "")
  }

  #reconcileTools(
    viewModel: TurnCardViewModel,
    previous: TurnCardViewModel | undefined,
    toolPool?: ToolBlockRenderable[],
  ): void {
    const retained = new Set(viewModel.tools.map((tool) => tool.toolCallId))
    const recyclableCards = toolPool ?? []
    for (const [toolCallId, card] of this.#toolCards) {
      if (retained.has(toolCallId)) continue
      if (card.parent === this) this.remove(card)
      this.#toolCards.delete(toolCallId)
      recyclableCards.push(card)
    }
    const previousTools = new Map(
      (previous?.tools ?? []).map((tool) => [tool.toolCallId, tool] as const),
    )
    for (const [index, tool] of viewModel.tools.entries()) {
      let card = this.#toolCards.get(tool.toolCallId)
      if (card === undefined) {
        card = recyclableCards.pop()
        if (card === undefined) {
          card = new ToolBlockRenderable(
            this.ctx,
            this.#theme,
            tool,
            viewModel.toolExpansion[index],
            (expanded) => this.#onToolExpansionChange(tool.toolCallId, expanded),
            {
              syntaxStyle: this.#syntaxStyle,
              ...(this.#treeSitterClient === undefined
                ? {}
                : { treeSitterClient: this.#treeSitterClient }),
              ...(this.#onOpenToolOutput === undefined
                ? {}
                : { onOpenToolOutput: this.#onOpenToolOutput }),
            },
          )
        } else {
          card.retarget(
            tool,
            viewModel.toolExpansion[index],
            (expanded) => this.#onToolExpansionChange(tool.toolCallId, expanded),
          )
        }
        this.#toolCards.set(tool.toolCallId, card)
        card.update(tool, viewModel.width)
      } else if (
        previousTools.get(tool.toolCallId) !== tool ||
        previous?.width !== viewModel.width ||
        previous?.rootsGeneration !== viewModel.rootsGeneration
      ) {
        card.update(tool, viewModel.width)
      }
    }
    const nextOrder = viewModel.tools.map((tool) => tool.toolCallId)
    const orderChanged =
      nextOrder.length !== this.#toolOrder.length ||
      nextOrder.some((toolCallId, index) => toolCallId !== this.#toolOrder[index])
    if (orderChanged) {
      let anchor: BaseRenderable | null = this.#subagentPanel
      for (let index = nextOrder.length - 1; index >= 0; index -= 1) {
        const card = this.#toolCards.get(nextOrder[index]!)
        if (card === undefined) continue
        if (card.parent === this) this.remove(card)
        if (anchor === null) this.add(card)
        else this.insertBefore(card, anchor)
        anchor = card
      }
      this.#toolOrder = nextOrder
    }
    if (toolPool === undefined) {
      for (const card of recyclableCards) card.destroyRecursively()
    }
  }

  releaseToolCards(pool: ToolBlockRenderable[]): void {
    for (const [toolCallId, card] of this.#toolCards) {
      if (card.parent === this) this.remove(card)
      this.#toolCards.delete(toolCallId)
      pool.push(card)
    }
    this.#toolOrder = []
  }

  #reconcileSubagents(viewModel: TurnCardViewModel): void {
    if (viewModel.visibleSubagents.length === 0) {
      if (this.#subagentPanel !== null) {
        if (this.#subagentPanel.parent === this) this.remove(this.#subagentPanel)
        this.#subagentPanel.destroyRecursively()
        this.#subagentPanel = null
      }
      return
    }
    if (this.#subagentPanel === null) {
      this.#subagentPanel = new SubagentPanelRenderable(
        this.ctx,
        this.#theme,
        this.#onOpenSubagent,
      )
      this.add(this.#subagentPanel)
    }
    this.#subagentPanel.update(viewModel.visibleSubagents, viewModel.subagentTotal)
  }

  #insertBeforeProjections(renderable: BaseRenderable): void {
    const firstToolId = this.#toolOrder[0]
    const anchor = firstToolId === undefined
      ? this.#subagentPanel
      : this.#toolCards.get(firstToolId) ?? this.#subagentPanel
    if (anchor === null) this.add(renderable)
    else this.insertBefore(renderable, anchor)
  }
}

function recyclablePlainEntry(viewModel: TurnCardViewModel): boolean {
  const presentation = viewModel.entry.presentation
  return (presentation === undefined || presentation === "conversation") &&
    turnReasoningMarkdown(viewModel.entry.turn) === "" &&
    viewModel.visibleSubagents.length === 0
}

function turnCardMarkdown(entry: TranscriptEntry, width: number): string {
  if (entry.presentation === "shell_result" && entry.shell !== undefined) return ""
  return terminalMarkdown(
    entry.presentation === "command_result" && entry.commandResult !== undefined
      ? commandResultMarkdown(entry.commandResult)
      : turnMarkdown(entry.turn),
    Math.max(20, width - 4),
  )
}

function shellHeader(active: boolean, status: number | null): string {
  if (active) return "◌ Shell · running"
  if (status === 0) return "✓ Shell · exited 0"
  if (status === null) return "■ Shell · finished"
  return `✕ Shell · exited ${status}`
}

export class TranscriptRenderable extends BoxRenderable {
  readonly scroller: ScrollBoxRenderable
  readonly emptyState: BoxRenderable
  readonly emptyStateTitle: TextRenderable
  readonly emptyStateHint: TextRenderable
  readonly streamingCard: BoxRenderable
  readonly streamingMarkdown: MarkdownRenderable
  readonly compactionCard: BoxRenderable
  readonly compactionMarkdown: MarkdownRenderable
  readonly mountedCards = new Map<string, TurnCardRenderable>()
  readonly #tailReasoning: ReasoningBlockRenderable
  readonly #compactionReasoning: ReasoningBlockRenderable
  readonly #tailHeader: TextRenderable
  readonly #compactionHeader: TextRenderable
  readonly #tailCitations: TextRenderable
  readonly #tailTools: BoxRenderable
  readonly #theme: RottweilerTheme
  readonly #syntaxStyle: SyntaxStyle
  readonly #treeSitterClient: TreeSitterClient | undefined
  readonly #onInteraction: (() => void) | undefined
  readonly #onOpenSubagent: ((subagentId: string) => void) | undefined
  readonly #onOpenToolOutput: ((toolCallId: string) => void) | undefined
  readonly #toolExpansion = new Map<string, boolean>()
  readonly #historicalToolPool: ToolBlockRenderable[] = []
  readonly #tailToolCards = new Map<string, ToolBlockRenderable>()
  readonly #tailToolPool: ToolBlockRenderable[] = []
  readonly #reasoningExpansion = new Map<string, boolean>()
  #selectedBlockId: string | null = null
  #state: RottweilerState | null = null
  #transcript: readonly TranscriptEntry[] | null = null
  #presentableTranscript: readonly TranscriptEntry[] = []
  #tools: RottweilerState["tools"] | null = null
  #turns: RottweilerState["turns"] | null = null
  #subagents: RottweilerState["subagents"] | null = null
  #workspaceRoots: RottweilerState["workspaceRoots"] | null = null
  #historicalTurnIds = new Set<string>()
  #agentName = "Rottweiler"
  #tailReasoningTurnId: string | null = null
  #compactionAttempt: number | null = null
  #recycledSinceCollection = 0

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    options: TranscriptRenderableOptions,
  ) {
    super(ctx, {
      id: "transcript",
      width: "100%",
      flexGrow: 1,
      minHeight: 1,
      backgroundColor: theme.background,
    })
    this.#theme = theme
    this.#syntaxStyle = options.syntaxStyle
    this.#treeSitterClient = options.treeSitterClient
    this.#onInteraction = options.onInteraction
    this.#onOpenSubagent = options.onOpenSubagent
    this.#onOpenToolOutput = options.onOpenToolOutput
    this.scroller = new ScrollBoxRenderable(ctx, {
      id: "transcript-scroll",
      width: "100%",
      height: "100%",
      scrollY: true,
      scrollX: false,
      stickyScroll: true,
      stickyStart: "bottom",
      scrollAcceleration: getScrollAcceleration(),
      viewportCulling: true,
      contentOptions: { flexDirection: "column", width: "100%" },
      verticalScrollbarOptions: { showArrows: false, trackOptions: { backgroundColor: theme.backgroundPanel } },
    })
    this.scroller.onMouseUp = () => this.#onInteraction?.()
    this.emptyState = new BoxRenderable(ctx, {
      id: "transcript-empty-state",
      width: "100%",
      minHeight: 6,
      flexGrow: 1,
      flexDirection: "column",
      justifyContent: "center",
      paddingX: 2,
      visible: false,
    })
    this.emptyStateTitle = new TextRenderable(ctx, {
      id: "transcript-empty-state-title",
      content: "rottweiler",
      fg: theme.primary,
      height: 1,
      flexShrink: 0,
      selectable: true,
    })
    this.emptyStateHint = new TextRenderable(ctx, {
      id: "transcript-empty-state-hint",
      content: "Describe a task, or press / for commands.",
      fg: theme.textMuted,
      height: 3,
      flexShrink: 0,
      wrapMode: "word",
      selectable: true,
    })
    this.emptyState.add(this.emptyStateTitle)
    this.emptyState.add(this.emptyStateHint)
    this.scroller.add(this.emptyState)
    this.streamingCard = new BoxRenderable(ctx, {
      id: "streaming-tail",
      width: "100%",
      minHeight: 2,
      flexDirection: "column",
      flexShrink: 0,
      backgroundColor: theme.background,
      paddingX: 0,
      paddingY: 0,
      marginTop: 1,
      visible: false,
    })
    this.#tailHeader = new TextRenderable(ctx, {
      content: "● rottweiler  streaming",
      fg: theme.text,
      height: 1,
      flexShrink: 0,
    })
    this.#tailReasoning = new ReasoningBlockRenderable(ctx, theme, options.syntaxStyle, {
      blockId: "reasoning:tail",
      content: "",
      expanded: true,
      streaming: true,
      width: Math.max(20, this.width || ctx.width),
      ...(options.treeSitterClient === undefined ? {} : { treeSitterClient: options.treeSitterClient }),
      onExpansionChange: () => {
        if (this.#state !== null) this.#updateTail(this.#state)
      },
      ...(this.#onInteraction === undefined ? {} : { onInteraction: this.#onInteraction }),
    })
    this.streamingMarkdown = new MarkdownRenderable(ctx, {
      id: "streaming-markdown",
      content: "",
      syntaxStyle: options.syntaxStyle,
      ...(options.treeSitterClient === undefined
        ? {}
        : { treeSitterClient: options.treeSitterClient }),
      fg: theme.markdownText,
      conceal: true,
      concealCode: false,
      streaming: true,
      width: "100%",
      marginLeft: 2,
      flexShrink: 0,
      // OpenTUI's incremental parser can retain every completed top-level
      // block and only reconcile the unstable trailing block. Keeping one
      // persistent MarkdownRenderable in this mode prevents the raw-markdown
      // flash and full document relayout that otherwise occurs on each token.
      internalBlockMode: "top-level",
      tableOptions: { style: "grid", widthMode: "full", wrapMode: "word" },
    })
    this.streamingMarkdown.selectable = true
    this.#tailCitations = new TextRenderable(ctx, {
      content: "",
      fg: theme.info,
      visible: false,
      wrapMode: "word",
      selectable: true,
    })
    this.#tailTools = new BoxRenderable(ctx, {
      id: "streaming-tools",
      width: "100%",
      flexDirection: "column",
      marginTop: 1,
    })
    this.streamingCard.add(this.#tailHeader)
    this.streamingCard.add(this.#tailReasoning)
    this.streamingCard.add(this.streamingMarkdown)
    this.streamingCard.add(this.#tailTools)
    this.streamingCard.add(this.#tailCitations)
    this.scroller.add(this.streamingCard)
    this.compactionCard = new BoxRenderable(ctx, {
      id: "compaction-stream",
      width: "100%",
      minHeight: 2,
      flexDirection: "column",
      flexShrink: 0,
      backgroundColor: theme.background,
      paddingX: 0,
      paddingY: 0,
      marginTop: 1,
      visible: false,
    })
    this.#compactionHeader = new TextRenderable(ctx, {
      content: "● Rottweiler · compacting context",
      fg: theme.accent,
      height: 1,
      flexShrink: 0,
    })
    this.#compactionReasoning = new ReasoningBlockRenderable(ctx, theme, options.syntaxStyle, {
      blockId: "reasoning:compaction",
      content: "",
      expanded: true,
      streaming: true,
      width: Math.max(20, this.width || ctx.width),
      ...(options.treeSitterClient === undefined ? {} : { treeSitterClient: options.treeSitterClient }),
      onExpansionChange: () => {
        if (this.#state !== null) this.#updateCompaction(this.#state)
      },
      ...(this.#onInteraction === undefined ? {} : { onInteraction: this.#onInteraction }),
    })
    this.compactionMarkdown = new MarkdownRenderable(ctx, {
      id: "compaction-markdown",
      content: "",
      syntaxStyle: options.syntaxStyle,
      ...(options.treeSitterClient === undefined
        ? {}
        : { treeSitterClient: options.treeSitterClient }),
      fg: theme.markdownText,
      conceal: true,
      concealCode: false,
      streaming: true,
      width: "100%",
      marginLeft: 2,
      flexShrink: 0,
      internalBlockMode: "top-level",
      tableOptions: { style: "grid", widthMode: "full", wrapMode: "word" },
    })
    this.compactionMarkdown.selectable = true
    this.compactionCard.add(this.#compactionHeader)
    this.compactionCard.add(this.#compactionReasoning)
    this.compactionCard.add(this.compactionMarkdown)
    this.scroller.add(this.compactionCard)
    this.add(this.scroller)
  }

  get mountedEntryCount(): number {
    return this.mountedCards.size
  }

  get mountedKeys(): readonly string[] {
    return this.#presentableTranscript.map(entryKey)
  }

  get selectedBlockId(): string | null {
    return this.#selectedBlockId
  }

  captureClientState(): TranscriptClientState {
    return {
      blocks: {
        selectedId: this.#selectedBlockId,
        expanded: this.#blocksInVisualOrder().map((block) => ({ id: block.blockId, expanded: block.expanded })),
      },
      tools: [...this.#toolExpansion].map(([id, expanded]) => ({ id, expanded })),
      reasoning: [...this.#reasoningExpansion].map(([id, expanded]) => ({ id, expanded })),
    }
  }

  restoreClientState(state: TranscriptClientState): boolean {
    for (const item of state.tools) this.#toolExpansion.set(item.id, item.expanded)
    for (const item of state.reasoning) this.#reasoningExpansion.set(item.id, item.expanded)
    this.#reconcileHistory()
    const blocks = new Map(this.#blocksInVisualOrder().map((block) => [block.blockId, block]))
    for (const item of state.blocks.expanded) {
      const block = blocks.get(item.id)
      if (block !== undefined && block.expanded !== item.expanded) block.toggle()
    }
    this.#selectedBlockId = state.blocks.selectedId
    this.#syncBlockSelection()
    return state.blocks.expanded.every((item) => blocks.has(item.id))
      && (state.blocks.selectedId === null || this.#selectedBlockId === state.blocks.selectedId)
  }

  selectNextBlock(): void {
    this.#syncBlockSelection()
    const blocks = this.#blocksInVisualOrder()
    if (blocks.length === 0) return
    const selectedIndex = blocks.findIndex((block) => block.blockId === this.#selectedBlockId)
    const nextIndex = selectedIndex < 0 ? 0 : Math.min(selectedIndex + 1, blocks.length - 1)
    this.#selectedBlockId = blocks[nextIndex]?.blockId ?? null
    this.#syncBlockSelection(true)
  }

  selectPreviousBlock(): void {
    this.#syncBlockSelection()
    const blocks = this.#blocksInVisualOrder()
    if (blocks.length === 0) return
    const selectedIndex = blocks.findIndex((block) => block.blockId === this.#selectedBlockId)
    const previousIndex = selectedIndex < 0 ? blocks.length - 1 : Math.max(selectedIndex - 1, 0)
    this.#selectedBlockId = blocks[previousIndex]?.blockId ?? null
    this.#syncBlockSelection(true)
  }

  toggleSelectedBlock(): void {
    this.#syncBlockSelection()
    const selected = this.#blocksInVisualOrder()
      .find((block) => block.blockId === this.#selectedBlockId)
    if (selected === undefined) return
    selected.toggle()
    this.#syncBlockSelection(true)
  }

  clearBlockSelection(): void {
    if (this.#selectedBlockId === null) return
    const selectedBlockId = this.#selectedBlockId
    this.#selectedBlockId = null
    this.#blocksInVisualOrder()
      .find((block) => block.blockId === selectedBlockId)
      ?.setSelected(false)
  }

  update(state: RottweilerState, agentName = "Rottweiler"): void {
    const previousStreamingTurnId = this.#state?.streamingTail?.turnId ?? null
    this.#state = state
    this.#agentName = truncateToCells(agentName.replace(/\s+/g, " ").trim(), 48) || "Child agent"
    const transcriptChanged = this.#transcript !== state.transcript
    if (transcriptChanged && state.streamingTail === null && this.#tailReasoningTurnId !== null) {
      const committed = [...state.transcript]
        .reverse()
        .find((entry) =>
          entry.agentTurn === this.#tailReasoningTurnId && entry.turn.role === "assistant")
      if (committed !== undefined) {
        this.#reasoningExpansion.set(entryKey(committed), this.#tailReasoning.expanded)
      }
    }
    const streamingTurnId = state.streamingTail?.turnId ?? null
    if (transcriptChanged || previousStreamingTurnId !== streamingTurnId) {
      this.#historicalTurnIds = new Set(
        state.transcript
          .map((entry) => entry.agentTurn)
          .filter((turnId) => turnId !== streamingTurnId),
      )
    }
    const historicalToolsChanged = this.#tools !== state.tools && toolProjectionChangedForHistory(
      this.#tools,
      state.tools,
      this.#historicalTurnIds,
    )
    const cardProjectionChanged =
      historicalToolsChanged ||
      this.#subagents !== state.subagents ||
      this.#workspaceRoots !== state.workspaceRoots
    const turnProjectionChanged = this.#turns !== state.turns
    this.#transcript = state.transcript
    this.#tools = state.tools
    this.#turns = state.turns
    this.#subagents = state.subagents
    this.#workspaceRoots = state.workspaceRoots
    if (transcriptChanged || cardProjectionChanged) {
      this.#presentableTranscript = presentableTranscript(state)
        .slice(-MAX_MOUNTED_TRANSCRIPT_ENTRIES)
    }
    this.emptyState.visible =
      !state.replay.active &&
      this.#presentableTranscript.length === 0 &&
      state.streamingTail === null &&
      !state.compaction.active
    if (this.emptyState.visible) {
      const workspace = state.workspaceStatus
      const workspaceLine = workspace === null
        ? ""
        : `${workspace.workspaceName}${workspace.branch === null ? "" : ` · ${workspace.branch}`} · ${workspace.changedPaths.length === 0 ? "clean" : `${workspace.changedPaths.length} changed`}`
      this.emptyStateHint.content = [
        workspaceLine,
        "Describe a task, or press / for commands.",
      ].filter(Boolean).join("\n")
    }
    this.#updateTail(state)
    this.#updateCompaction(state)
    if (transcriptChanged || cardProjectionChanged || turnProjectionChanged) {
      this.#reconcileHistory()
    }
    this.#syncBlockSelection()
  }

  setScrollOffset(scrollTop: number): void {
    this.scroller.scrollTo(scrollTop)
  }

  scrollBy(direction: 1 | -1, unit: "step" | "viewport"): void {
    this.scroller.scrollBy(direction, unit)
  }

  scrollTo(position: number): void {
    this.scroller.scrollTo(position)
  }

  protected override onResize(_width: number, _height: number): void {
    if (this.#state !== null) {
      this.#updateTail(this.#state)
      this.#updateCompaction(this.#state)
    }
    this.#reconcileHistory()
  }

  #reconcileHistory(): void {
    const state = this.#state
    if (state === null) return
    const width = Math.max(20, this.width || this.ctx.width)
    const transcript = this.#presentableTranscript
    const desiredKeys = new Set(transcript.map(entryKey))
    const recyclableCards: TurnCardRenderable[] = []
    const recyclableToolCards = this.#historicalToolPool
    for (const [key, card] of this.mountedCards) {
      if (desiredKeys.has(key)) continue
      this.scroller.remove(card)
      this.mountedCards.delete(key)
      if (recyclablePlainEntry(card.viewModel)) recyclableCards.push(card)
      else {
        card.releaseToolCards(recyclableToolCards)
        card.destroyRecursively()
      }
    }
    const toolEntryKeys = projectionEntryKeys(transcript, "tool")
    const subagentEntryKeys = projectionEntryKeys(transcript, "assistant")
    const lastAssistantEntryByTurn = new Map<string, string>()
    for (const transcriptEntry of transcript) {
      if (transcriptEntry.turn.role === "assistant") {
        lastAssistantEntryByTurn.set(transcriptEntry.agentTurn, entryKey(transcriptEntry))
      }
    }
    let reference: BoxRenderable = this.streamingCard
    for (let index = transcript.length - 1; index >= 0; index -= 1) {
      const entry = transcript[index]
      if (entry === undefined) continue
      const key = entryKey(entry)
      const candidateTools = toolEntryKeys.has(key)
        ? Object.values(state.tools).filter((tool) => tool.turnId === entry.agentTurn)
        : []
      const turnSubagents = subagentEntryKeys.has(key)
        ? subagentsForTurn(state, entry.agentTurn).filter(
            (subagent) => subagent.status !== "running",
          )
        : []
      const candidateSubagents = boundedSubagents(turnSubagents)
      const detail = entry.turn.role === "assistant" &&
          lastAssistantEntryByTurn.get(entry.agentTurn) === key &&
          state.turns[entry.agentTurn]?.cost != null
        ? turnDetail(state.turns[entry.agentTurn]?.cost, state.turns[entry.agentTurn]?.usage)
        : null
      const retained = this.mountedCards.get(key)
      const previous = retained?.viewModel
      const tools = reuseReferenceArray(previous?.tools, candidateTools)
      const visibleSubagents = reuseReferenceArray(
        previous?.visibleSubagents,
        candidateSubagents,
      )
      const toolExpansion = reuseReferenceArray(
        previous?.toolExpansion,
        tools.map((tool) => this.#toolExpansion.get(tool.toolCallId)),
      )
      const candidateViewModel: TurnCardViewModel = {
        key,
        first: index === 0,
        width,
        entry,
        detail,
        tools,
        visibleSubagents,
        subagentTotal: turnSubagents.length,
        toolExpansion,
        reasoningExpanded: this.#reasoningExpansion.get(key) ?? true,
        rootsGeneration: state.workspaceRoots?.generation ?? "",
      }
      const viewModel = previous !== undefined && sameTurnCardViewModel(previous, candidateViewModel)
        ? previous
        : candidateViewModel
      if (retained !== undefined) {
        retained.update(viewModel, recyclableToolCards)
        reference = retained
        continue
      }
      const recycled = recyclablePlainEntry(viewModel)
        ? recyclableCards.pop()
        : undefined
      const card = recycled !== undefined && recycled.canRecycleFor(viewModel)
        ? recycled
        : new TurnCardRenderable(
            this.ctx,
            this.#theme,
            this.#syntaxStyle,
            viewModel,
            (toolCallId, expanded) => this.#rememberToolExpansion(toolCallId, expanded),
            (expanded) => this.#rememberReasoningExpansion(key, expanded),
            this.#onInteraction,
            this.#onOpenSubagent,
            this.#onOpenToolOutput,
            this.#treeSitterClient,
          )
      if (card === recycled) {
        card.update(viewModel, recyclableToolCards)
        this.#recycledSinceCollection += 1
      }
      this.scroller.insertBefore(card, reference)
      this.mountedCards.set(key, card)
      reference = card
    }
    for (const card of recyclableCards) {
      card.releaseToolCards(recyclableToolCards)
      card.destroyRecursively()
    }
    while (recyclableToolCards.length > 16) recyclableToolCards.shift()?.destroyRecursively()
    if (this.#recycledSinceCollection >= 64) {
      // Rebinding Markdown renderables releases their prior incremental parse
      // trees, but Bun may otherwise defer tracing those detached token graphs
      // indefinitely in a continuously active terminal. Collect only after a
      // full batch has accumulated, outside the per-frame streaming path.
      Bun.gc(true)
      this.#recycledSinceCollection = 0
    }
    this.#syncBlockSelection()
  }

  #updateTail(state: RottweilerState): void {
    const tail = state.streamingTail
    this.streamingCard.visible = tail !== null
    if (tail === null) {
      // Finalize the trailing parser block before clearing it. This mirrors
      // OpenCode's streaming lifecycle and avoids leaving an unfinished code
      // fence/table parse state behind when the next response starts.
      this.streamingMarkdown.streaming = false
      this.streamingMarkdown.content = ""
      this.streamingMarkdown.visible = false
      this.#tailHeader.content = `${this.#agentName} · delegating`
      this.#tailReasoning.update("", false, Math.max(20, this.width || this.ctx.width))
      this.#tailReasoningTurnId = null
      this.#tailCitations.visible = false
      this.#replaceTailTools([])
      return
    }
    this.#tailReasoning.setBlockId(`reasoning:tail:${tail.turnId}`)
    const tools = tail.toolCallIds
      .map((toolCallId) => state.tools[toolCallId])
      .filter((tool): tool is ToolProjection => tool !== undefined)
    const reasoning = presentableReasoning(tail.thinking + (tail.displayBudget.thinking.omittedBytes > 0 ? `\n${DISPLAY_TRUNCATION_MARKER}` : ""))
    const activity = tools.some((tool) => tool.status === "awaiting_approval")
      ? "Waiting for approval"
      : tools.some((tool) => tool.status === "running")
        ? "Running tools"
        : reasoning !== ""
          ? "Thinking"
          : "Streaming"
    // Set the parser mode before appending content. Reversing this order makes
    // the new chunk take the non-streaming parse path for one frame, which is
    // the visible plain-text-then-Markdown flicker reported by users.
    this.streamingMarkdown.streaming = tail.finished === null
    this.streamingMarkdown.visible = tail.text.length > 0
    this.streamingMarkdown.content = terminalMarkdown(
      tail.text + (tail.displayBudget.text.omittedBytes > 0 ? `\n${DISPLAY_TRUNCATION_MARKER}` : ""),
      Math.max(20, (this.width || this.ctx.width) - 4),
      tail.finished === null ? "streaming" : "complete",
    )
    const detail = tail.finished === null
      ? state.model ?? activity.toLowerCase()
      : turnDetail(tail.finished.cost, tail.finished.usage)
    this.#tailHeader.content = t`${fg(this.#theme.accent)("● ")}${bold(fg(this.#theme.text)(this.#agentName.toLowerCase()))}${fg(this.#theme.textMuted)(`  ${detail}`)}`
    if (this.#tailReasoningTurnId !== tail.turnId) {
      this.#tailReasoning.expand(false)
      this.#tailReasoningTurnId = tail.turnId
    }
    this.#tailReasoning.update(
      reasoning,
      tail.finished === null,
      Math.max(20, this.width || this.ctx.width),
    )
    this.streamingMarkdown.marginTop = reasoning === "" || tail.text.length === 0 ? 0 : 1
    this.#tailCitations.visible = tail.citations.length > 0
    this.#tailCitations.content = tail.citations
      .map((citation, index) => `[${index + 1}] ${citation.title ?? citation.uri}`)
      .join("  ")
    this.#tailCitations.height = tail.citations.length > 0 ? 1 : 0
    this.#tailCitations.marginLeft = 2
    this.#tailCitations.marginTop = tail.citations.length > 0 ? 1 : 0
    this.#tailTools.marginTop = tools.length > 0 && tail.text.length > 0 ? 1 : 0
    this.#replaceTailTools(tools)
  }

  #updateCompaction(state: RottweilerState): void {
    const compaction = state.compaction
    this.compactionCard.visible = compaction.active &&
      (compaction.text !== "" || compaction.thinking !== "")
    if (!compaction.active) {
      this.compactionMarkdown.streaming = false
      this.compactionMarkdown.content = ""
      this.compactionMarkdown.visible = false
      this.#compactionReasoning.update("", false, Math.max(20, this.width || this.ctx.width))
      this.#compactionAttempt = null
      return
    }
    this.#compactionReasoning.setBlockId(`reasoning:compaction:${compaction.attempt}`)
    if (this.#compactionAttempt !== compaction.attempt) {
      this.#compactionReasoning.expand(false)
      this.#compactionAttempt = compaction.attempt
    }
    const width = Math.max(20, (this.width || this.ctx.width) - 4)
    this.compactionMarkdown.streaming = true
    this.compactionMarkdown.visible = compaction.text !== ""
    this.compactionMarkdown.content = terminalMarkdown(compaction.text, width, "streaming")
    this.#compactionReasoning.update(
      compaction.thinking,
      true,
      Math.max(20, this.width || this.ctx.width),
    )
    this.#compactionHeader.content = compaction.text === "" && compaction.thinking === ""
      ? "Rottweiler · compacting context"
      : "Rottweiler · summarizing context"
  }

  #replaceTailTools(tools: readonly ToolProjection[]): void {
    const retained = new Set(tools.map((tool) => tool.toolCallId))
    for (const [toolCallId, card] of this.#tailToolCards) {
      if (retained.has(toolCallId)) continue
      this.#tailTools.remove(card)
      this.#tailToolCards.delete(toolCallId)
      this.#tailToolPool.push(card)
    }
    for (const tool of tools) {
      let card = this.#tailToolCards.get(tool.toolCallId)
      if (card === undefined) {
        card = this.#tailToolPool.pop()
        if (card === undefined) {
          card = new ToolBlockRenderable(
            this.ctx,
            this.#theme,
            tool,
            this.#toolExpansion.get(tool.toolCallId),
            (expanded) => this.#rememberToolExpansion(tool.toolCallId, expanded),
            {
              syntaxStyle: this.#syntaxStyle,
              ...(this.#treeSitterClient === undefined
                ? {}
                : { treeSitterClient: this.#treeSitterClient }),
              ...(this.#onOpenToolOutput === undefined
                ? {}
                : { onOpenToolOutput: this.#onOpenToolOutput }),
            },
          )
        } else {
          card.retarget(
            tool,
            this.#toolExpansion.get(tool.toolCallId),
            (expanded) => this.#rememberToolExpansion(tool.toolCallId, expanded),
          )
        }
        card.update(tool, Math.max(20, this.width || this.ctx.width))
        this.#tailToolCards.set(tool.toolCallId, card)
        this.#tailTools.add(card)
      } else {
        card.update(tool, Math.max(20, this.width || this.ctx.width))
      }
    }
    while (this.#tailToolPool.length > 16) this.#tailToolPool.shift()?.destroyRecursively()
  }

  override destroy(): void {
    for (const card of this.#historicalToolPool) card.destroyRecursively()
    this.#historicalToolPool.length = 0
    for (const card of this.#tailToolPool) card.destroyRecursively()
    this.#tailToolPool.length = 0
    super.destroy()
  }

  #rememberToolExpansion(toolCallId: string, expanded: boolean): void {
    if (this.#toolExpansion.get(toolCallId) === expanded) return
    this.#toolExpansion.set(toolCallId, expanded)
    const state = this.#state
    if (state === null) return
    this.#updateTail(state)
  }

  #rememberReasoningExpansion(key: string, expanded: boolean): void {
    if ((this.#reasoningExpansion.get(key) ?? false) === expanded) return
    this.#reasoningExpansion.set(key, expanded)
  }

  #blocksInVisualOrder(): Array<ToolBlockRenderable | ReasoningBlockRenderable> {
    const blocks: Array<ToolBlockRenderable | ReasoningBlockRenderable> = []
    const visit = (renderable: BaseRenderable): void => {
      if (!renderable.visible) return
      if (
        renderable instanceof ToolBlockRenderable ||
        renderable instanceof ReasoningBlockRenderable
      ) {
        blocks.push(renderable)
        return
      }
      for (const child of renderable.getChildren()) visit(child)
    }
    for (const child of this.scroller.getChildren()) visit(child)
    return blocks
  }

  #syncBlockSelection(ensureVisible = false): void {
    if (this.#selectedBlockId === null) return
    const blocks = this.#blocksInVisualOrder()
    const selected = blocks.find((block) => block.blockId === this.#selectedBlockId)
    if (selected === undefined) {
      this.#selectedBlockId = null
    }
    for (const block of blocks) block.setSelected(block === selected)
    if (ensureVisible && selected !== undefined) {
      this.scroller.scrollChildIntoView(selected.header.id)
    }
  }
}

function toolProjectionChangedForHistory(
  previous: RottweilerState["tools"] | null,
  next: RottweilerState["tools"],
  historicalTurns: ReadonlySet<string>,
): boolean {
  if (previous === null) return true
  const ids = new Set([...Object.keys(previous), ...Object.keys(next)])
  for (const id of ids) {
    const before = previous[id]
    const after = next[id]
    if (before === after) continue
    if (
      (before !== undefined && historicalTurns.has(before.turnId)) ||
      (after !== undefined && historicalTurns.has(after.turnId))
    ) return true
  }
  return false
}

function turnDetail(
  cost: Parameters<typeof formatCost>[0],
  usage: Parameters<typeof formatCost>[1],
): string {
  const detail = formatCost(cost, usage)
  return cost?.kind === "subscription_quota" && (cost.used === null || cost.used === undefined)
    ? `turn usage · ${detail}`
    : detail
}

function projectionEntryKeys(
  transcript: readonly TranscriptEntry[],
  preferredRole: TranscriptEntry["turn"]["role"],
): Set<string> {
  const fallback = new Map<string, TranscriptEntry>()
  const preferred = new Map<string, TranscriptEntry>()
  for (const entry of transcript) {
    fallback.set(entry.agentTurn, entry)
    if (entry.turn.role === preferredRole) preferred.set(entry.agentTurn, entry)
  }
  return new Set(
    [...fallback.keys()].map((turnId) => entryKey(preferred.get(turnId) ?? fallback.get(turnId)!)),
  )
}

/**
 * Provider continuation state is durable but not user-facing. Do not mount an
 * empty assistant card merely because a committed turn retained an encrypted
 * reasoning signature. Tool and subagent-only turns remain visible through
 * their dedicated projections.
 */
function presentableTranscript(state: RottweilerState): TranscriptEntry[] {
  const toolTurns = new Set(Object.values(state.tools).map((tool) => tool.turnId))
  const subagentTurns = new Set(
    Object.values(state.subagents).map((subagent) => subagent.parentTurnId),
  )
  return state.transcript.filter((entry) => {
    if (entry.presentation === "shell_result" && entry.shell !== undefined) return true
    if (entry.presentation === "command_result" && entry.commandResult !== undefined) return true
    if (turnMarkdown(entry.turn).trim() !== "") return true
    if (turnReasoningMarkdown(entry.turn) !== "") return true
    if (entry.turn.role === "tool" && toolTurns.has(entry.agentTurn)) return true
    if (entry.turn.role === "assistant" && subagentTurns.has(entry.agentTurn)) return true
    return false
  })
}

function subagentsForTurn(state: RottweilerState, turnId: string): SubagentProjection[] {
  return state.subagentOrder
    .map((subagentId) => state.subagents[subagentId])
    .filter(
      (subagent): subagent is SubagentProjection =>
        subagent !== undefined && subagent.parentTurnId === turnId,
    )
}

function boundedSubagents(subagents: readonly SubagentProjection[]): SubagentProjection[] {
  if (subagents.length <= MAX_VISIBLE_SUBAGENTS) return [...subagents]
  const running = subagents.filter((subagent) => subagent.status === "running")
  const completed = subagents.filter((subagent) => subagent.status !== "running")
  const visibleRunning = running.slice(0, MAX_VISIBLE_SUBAGENTS)
  const remaining = MAX_VISIBLE_SUBAGENTS - visibleRunning.length
  if (remaining === 0) return visibleRunning
  return [...visibleRunning, ...completed.slice(-remaining)]
}
