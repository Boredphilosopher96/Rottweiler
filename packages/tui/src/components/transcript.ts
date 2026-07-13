import {
  BoxRenderable,
  MarkdownRenderable,
  ScrollBoxRenderable,
  TextRenderable,
  type RenderContext,
  type SyntaxStyle,
  type TreeSitterClient,
} from "@opentui/core"

import { estimateEntryHeight, entryKey, formatCost, formatToolArguments, toolOutputText, TranscriptVirtualizer, turnMarkdown } from "../render"
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
  readonly overscan?: number
}

const MAX_VISIBLE_SUBAGENTS = 8

export class ToolBlockRenderable extends BoxRenderable {
  readonly header: TextRenderable
  readonly body: TextRenderable
  #collapsed: boolean
  #tool: ToolProjection
  #theme: RottweilerTheme
  #onExpansionChange: ((expanded: boolean) => void) | undefined

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    tool: ToolProjection,
    expanded?: boolean,
    onExpansionChange?: (expanded: boolean) => void,
  ) {
    super(ctx, {
      id: `tool-${tool.toolCallId}`,
      width: "100%",
      minHeight: 1,
      flexDirection: "column",
      border: false,
      backgroundColor: theme.background,
      focusable: true,
      paddingX: 0,
      marginTop: 0,
    })
    this.#theme = theme
    this.#tool = tool
    this.#collapsed = expanded === undefined ? tool.status !== "awaiting_approval" : !expanded
    this.#onExpansionChange = onExpansionChange
    this.header = new TextRenderable(ctx, { content: "", fg: theme.foreground, height: 1 })
    this.body = new TextRenderable(ctx, {
      content: "",
      fg: theme.muted,
      wrapMode: "word",
      visible: !this.#collapsed,
    })
    this.onKeyDown = (key) => {
      if (key.name === "return" || key.name === "space") {
        key.preventDefault()
        this.toggle()
      }
    }
    this.onMouseDown = () => this.toggle()
    this.add(this.header)
    this.add(this.body)
    this.update(tool)
  }

  update(tool: ToolProjection): void {
    this.#tool = tool
    const glyph = tool.status === "awaiting_approval" ? "?" : tool.status === "running" ? "◌" : tool.isError === true ? "✕" : "✓"
    const approval = tool.status === "awaiting_approval" ? " · approval needed" : ""
    const args = compactToolArguments(tool)
    const result =
      tool.status === "finished" && this.#collapsed
        ? compactToolResult(tool)
        : ""
    this.header.content = `${this.#collapsed ? "›" : "⌄"} ${glyph} ${tool.name}${args === "" ? "" : ` ${args}`}${approval}${result === "" ? "" : `  ${result}`}`
    this.header.fg =
      tool.status === "awaiting_approval"
        ? this.#theme.warning
        : tool.isError === true
          ? this.#theme.danger
          : tool.status === "finished"
            ? this.#theme.success
            : this.#theme.info
    if (this.#collapsed) {
      this.body.content = ""
      this.body.height = 0
      this.height = 1
      return
    }
    this.body.content = toolBodyContent(tool)
    const bodyRows = Math.min(8, Math.max(1, this.body.plainText.split("\n").length))
    this.body.height = bodyRows
    this.height = bodyRows + 1
  }

  toggle(): void {
    this.#collapsed = !this.#collapsed
    this.body.visible = !this.#collapsed
    this.update(this.#tool)
    this.#onExpansionChange?.(!this.#collapsed)
  }
}

function toolBodyContent(tool: ToolProjection): string {
  const live = tool.chunks.map((chunk) => chunk.chunk).join("")
  const final = toolOutputText(tool.output)
  const output = tool.status === "finished" && final !== "" ? final : live
  const activity = tool.status === "awaiting_approval"
    ? "Awaiting approval…"
    : tool.status === "running"
      ? "Running…"
      : final === "" && live === ""
        ? "Completed with no output."
        : ""
  return [`Arguments · ${formatToolArguments(tool.args)}`, tool.rationale, output, activity]
    .filter(Boolean)
    .join("\n")
}

function toolBlockExpanded(
  tool: ToolProjection,
  expansion?: ReadonlyMap<string, boolean>,
): boolean {
  return expansion?.get(tool.toolCallId) ?? tool.status === "awaiting_approval"
}

function toolBlockRows(
  tool: ToolProjection,
  expansion?: ReadonlyMap<string, boolean>,
): number {
  if (!toolBlockExpanded(tool, expansion)) return 1
  return Math.min(8, Math.max(1, toolBodyContent(tool).split("\n").length)) + 1
}

function compactToolArguments(tool: ToolProjection): string {
  if (!isRecord(tool.args)) return formatToolArguments(tool.args, 64)
  const path = typeof tool.args.path === "string" ? tool.args.path : ""
  switch (tool.name) {
    case "ls":
      return path === "" ? "." : path
    case "glob": {
      const pattern = typeof tool.args.pattern === "string" ? tool.args.pattern : ""
      return `${pattern}${path === "" || path === "." ? "" : ` in ${path}`}`
    }
    case "read":
      return path
    case "bash":
    case "shell":
      return typeof tool.args.command === "string" ? singleLine(tool.args.command, 64) : ""
    default:
      return formatToolArguments(tool.args, 64)
  }
}

function compactToolResult(tool: ToolProjection): string {
  const result = toolOutputText(tool.output).trim()
  if (result === "") return tool.isError === true ? "Failed" : "Done"
  const lines = result.split("\n").filter(Boolean)
  if (lines.length > 1) return `${lines.length} lines · ${singleLine(lines[0] ?? "", 36)}`
  return singleLine(result, 40)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}

export class SubagentPanelRenderable extends BoxRenderable {
  readonly header: TextRenderable
  readonly rows = new Map<string, TextRenderable>()
  readonly #theme: RottweilerTheme
  #rowOrder: readonly string[] = []

  constructor(ctx: RenderContext, theme: RottweilerTheme) {
    super(ctx, {
      id: "subagent-progress",
      width: "100%",
      height: 0,
      flexDirection: "column",
      flexShrink: 0,
      border: true,
      borderStyle: "single",
      borderColor: theme.border,
      backgroundColor: theme.panel,
      paddingX: 1,
      marginTop: 1,
      visible: false,
    })
    this.#theme = theme
    this.header = new TextRenderable(ctx, {
      content: "",
      fg: theme.accentStrong,
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
    this.header.content = `Subagents · ${running} running · ${total} total`
    for (const [index, subagent] of subagents.entries()) {
      let row = this.rows.get(subagent.projectionId)
      if (row === undefined) {
        row = new TextRenderable(this.ctx, {
          content: "",
          fg: this.#theme.muted,
          height: 1,
          flexShrink: 0,
        })
        this.rows.set(subagent.projectionId, row)
        this.add(row)
      }
      const glyph = subagentGlyph(subagent.status)
      const branch = index === subagents.length - 1 ? "└─" : "├─"
      const detail = subagentDetail(subagent)
      row.content = `${branch} ${glyph} ${singleLine(subagent.task, 72)}${detail === "" ? "" : ` · ${detail}`}`
      row.fg =
        subagent.status === "failed"
          ? this.#theme.danger
          : subagent.status === "completed"
            ? this.#theme.success
            : subagent.status === "cancelled" ||
                subagent.status === "timed_out" ||
                subagent.status === "max_turns"
              ? this.#theme.warning
            : this.#theme.info
    }
    this.visible = subagents.length > 0
    this.height = subagents.length === 0 ? 0 : subagents.length + 3
  }
}

function subagentDetail(subagent: SubagentProjection): string {
  if (subagent.status === "running") {
    return subagent.activity ?? "starting"
  }
  const files = subagent.touchedFileCount === 0 ? "" : ` · ${subagent.touchedFileCount} files`
  const diff = subagent.diffArtifactId === null ? "" : " · diff ready"
  return `${subagent.summary === null ? subagent.status.replaceAll("_", " ") : singleLine(subagent.summary, 72)}${files}${diff}`
}

function subagentGlyph(status: SubagentProjection["status"]): string {
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

function singleLine(value: string, limit: number): string {
  const compact = value.replace(/\s+/g, " ").trim()
  return compact.length <= limit ? compact : `${compact.slice(0, Math.max(1, limit - 1))}…`
}

class TurnCardRenderable extends BoxRenderable {
  readonly header: TextRenderable
  readonly markdown: MarkdownRenderable

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    syntaxStyle: SyntaxStyle,
    entry: TranscriptEntry,
    width: number,
    cost: string,
    tools: readonly ToolProjection[],
    subagents: readonly SubagentProjection[],
    subagentTotal: number,
    toolExpansion: Map<string, boolean>,
    onToolExpansionChange: (toolCallId: string, expanded: boolean) => void,
    treeSitterClient?: TreeSitterClient,
  ) {
    const markdown = turnMarkdown(entry.turn)
    const toolOnly = entry.turn.role === "tool" && markdown === ""
    const role = entry.presentation === "command_result"
      ? "Command result"
      : entry.turn.role === "assistant"
        ? "Rottweiler"
        : entry.turn.role === "user"
          ? "You"
          : "Tools"
    super(ctx, {
      id: `turn-${entryKey(entry)}`,
      width: "100%",
      height:
        (toolOnly ? 0 : estimateEntryHeight(entry, width) + 1) +
        tools.reduce((rows, tool) => rows + toolBlockRows(tool, toolExpansion), 0) +
        (subagents.length === 0 ? 0 : subagents.length + 3),
      flexDirection: "column",
      flexShrink: 0,
      border: false,
      backgroundColor: entry.turn.role === "user" ? theme.panelRaised : theme.background,
      paddingX: 1,
      paddingY: toolOnly ? 0 : 1,
    })
    this.header = new TextRenderable(ctx, {
      content: entry.presentation === "command_result"
        ? `${role} · ${entry.title ?? "completed"}`
        : `${role} · ${cost}`,
      fg: entry.turn.role === "assistant" ? theme.accentStrong : theme.info,
      height: toolOnly ? 0 : 1,
      flexShrink: 0,
      visible: !toolOnly,
    })
    this.markdown = new MarkdownRenderable(ctx, {
      id: `markdown-${entryKey(entry)}`,
      content: markdown,
      syntaxStyle,
      ...(treeSitterClient === undefined ? {} : { treeSitterClient }),
      fg: theme.foreground,
      conceal: true,
      concealCode: false,
      streaming: false,
      width: "100%",
      flexGrow: 1,
      visible: !toolOnly,
      internalBlockMode: "top-level",
      tableOptions: { style: "columns", widthMode: "full", wrapMode: "word" },
    })
    if (!toolOnly) {
      this.add(this.header)
      this.add(this.markdown)
    }
    for (const tool of tools) {
      this.add(new ToolBlockRenderable(
        ctx,
        theme,
        tool,
        toolExpansion.get(tool.toolCallId),
        (expanded) => onToolExpansionChange(tool.toolCallId, expanded),
      ))
    }
    if (subagents.length > 0) {
      const panel = new SubagentPanelRenderable(ctx, theme)
      panel.update(subagents, subagentTotal)
      this.add(panel)
    }
  }
}

export class TranscriptRenderable extends BoxRenderable {
  readonly scroller: ScrollBoxRenderable
  readonly streamingCard: BoxRenderable
  readonly streamingMarkdown: MarkdownRenderable
  readonly subagentPanel: SubagentPanelRenderable
  readonly mountedCards = new Map<string, TurnCardRenderable>()
  readonly #topSpacer: BoxRenderable
  readonly #bottomSpacer: BoxRenderable
  readonly #tailThinking: TextRenderable
  readonly #tailHeader: TextRenderable
  readonly #tailCitations: TextRenderable
  readonly #tailTools: BoxRenderable
  readonly #virtualizer: TranscriptVirtualizer
  readonly #theme: RottweilerTheme
  readonly #syntaxStyle: SyntaxStyle
  readonly #treeSitterClient: TreeSitterClient | undefined
  readonly #toolExpansion = new Map<string, boolean>()
  #toolExpansionRevision = 0
  #virtualizedToolExpansionRevision = -1
  #state: RottweilerState | null = null
  #transcript: readonly TranscriptEntry[] | null = null
  #tools: RottweilerState["tools"] | null = null
  #turns: RottweilerState["turns"] | null = null
  #subagents: RottweilerState["subagents"] | null = null
  #virtualizedTranscript: readonly TranscriptEntry[] | null = null
  #virtualizedTools: RottweilerState["tools"] | null = null
  #virtualizedSubagents: RottweilerState["subagents"] | null = null
  #virtualizedWidth = 0
  #lastWindow = ""

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
    this.#virtualizer = new TranscriptVirtualizer(options.overscan)
    this.scroller = new ScrollBoxRenderable(ctx, {
      id: "transcript-scroll",
      width: "100%",
      height: "100%",
      scrollY: true,
      scrollX: false,
      stickyScroll: true,
      stickyStart: "bottom",
      viewportCulling: true,
      contentOptions: { flexDirection: "column", width: "100%" },
      verticalScrollbarOptions: { showArrows: false, trackOptions: { backgroundColor: theme.panel } },
    })
    this.#topSpacer = new BoxRenderable(ctx, { width: "100%", height: 0, flexShrink: 0 })
    this.#bottomSpacer = new BoxRenderable(ctx, { width: "100%", height: 0, flexShrink: 0 })
    this.streamingCard = new BoxRenderable(ctx, {
      id: "streaming-tail",
      width: "100%",
      minHeight: 2,
      flexDirection: "column",
      flexShrink: 0,
      backgroundColor: theme.background,
      paddingX: 1,
      paddingY: 1,
      visible: false,
    })
    this.#tailHeader = new TextRenderable(ctx, {
      content: "Rottweiler · streaming",
      fg: theme.accentStrong,
      height: 1,
      flexShrink: 0,
    })
    this.#tailThinking = new TextRenderable(ctx, {
      content: "",
      fg: theme.subtle,
      visible: false,
      wrapMode: "word",
    })
    this.streamingMarkdown = new MarkdownRenderable(ctx, {
      id: "streaming-markdown",
      content: "",
      syntaxStyle: options.syntaxStyle,
      ...(options.treeSitterClient === undefined
        ? {}
        : { treeSitterClient: options.treeSitterClient }),
      fg: theme.foreground,
      conceal: true,
      streaming: true,
      width: "100%",
    })
    this.#tailCitations = new TextRenderable(ctx, {
      content: "",
      fg: theme.info,
      visible: false,
      wrapMode: "word",
    })
    this.#tailTools = new BoxRenderable(ctx, {
      id: "streaming-tools",
      width: "100%",
      flexDirection: "column",
    })
    this.subagentPanel = new SubagentPanelRenderable(ctx, theme)
    this.streamingCard.add(this.#tailHeader)
    this.streamingCard.add(this.#tailThinking)
    this.streamingCard.add(this.streamingMarkdown)
    this.streamingCard.add(this.#tailCitations)
    this.streamingCard.add(this.subagentPanel)
    this.streamingCard.add(this.#tailTools)
    this.scroller.add(this.#topSpacer)
    this.scroller.add(this.#bottomSpacer)
    this.scroller.add(this.streamingCard)
    this.add(this.scroller)
  }

  get mountedEntryCount(): number {
    return this.mountedCards.size
  }

  get mountedKeys(): readonly string[] {
    return [...this.mountedCards.keys()]
  }

  update(state: RottweilerState): void {
    this.#state = state
    const transcriptChanged = this.#transcript !== state.transcript
    const cardProjectionChanged = this.#tools !== state.tools || this.#subagents !== state.subagents
    const turnProjectionChanged = this.#turns !== state.turns
    this.#transcript = state.transcript
    this.#tools = state.tools
    this.#turns = state.turns
    this.#subagents = state.subagents
    this.#updateTail(state)
    this.#reconcile(
      transcriptChanged ||
        turnProjectionChanged ||
        (state.streamingTail === null && cardProjectionChanged),
    )
  }

  setScrollOffset(scrollTop: number): void {
    this.scroller.scrollTop = scrollTop
    this.#reconcile(true)
  }

  protected override onResize(_width: number, _height: number): void {
    this.#reconcile(true)
  }

  #reconcile(force: boolean): void {
    const state = this.#state
    if (state === null) {
      return
    }
    const width = Math.max(20, this.width || this.ctx.width)
    const height = Math.max(4, this.height || this.ctx.height - 8)
    if (
      this.#virtualizedTranscript !== state.transcript ||
      this.#virtualizedTools !== state.tools ||
      this.#virtualizedSubagents !== state.subagents ||
      this.#virtualizedToolExpansionRevision !== this.#toolExpansionRevision ||
      this.#virtualizedWidth !== width
    ) {
      const toolRowsByTurn = new Map<string, number>()
      for (const tool of Object.values(state.tools)) {
        toolRowsByTurn.set(
          tool.turnId,
          (toolRowsByTurn.get(tool.turnId) ?? 0) + toolBlockRows(tool, this.#toolExpansion),
        )
      }
      const subagentRowsByTurn = new Map<string, number>()
      const subagentsByTurn = new Map<string, SubagentProjection[]>()
      for (const subagent of Object.values(state.subagents)) {
        const group = subagentsByTurn.get(subagent.parentTurnId) ?? []
        group.push(subagent)
        subagentsByTurn.set(subagent.parentTurnId, group)
      }
      for (const [turnId, subagents] of subagentsByTurn) {
        const visible = boundedSubagents(subagents)
        if (visible.length > 0) {
          subagentRowsByTurn.set(
            turnId,
            visible.length + 3,
          )
        }
      }
      const toolEntryKeys = projectionEntryKeys(state.transcript, "tool")
      const subagentEntryKeys = projectionEntryKeys(state.transcript, "assistant")
      this.#virtualizer.update(
        state.transcript,
        width,
        (entry) =>
          (toolEntryKeys.has(entryKey(entry)) ? (toolRowsByTurn.get(entry.agentTurn) ?? 0) : 0) +
          (subagentEntryKeys.has(entryKey(entry)) ? (subagentRowsByTurn.get(entry.agentTurn) ?? 0) : 0),
      )
      this.#virtualizedTranscript = state.transcript
      this.#virtualizedTools = state.tools
      this.#virtualizedSubagents = state.subagents
      this.#virtualizedToolExpansionRevision = this.#toolExpansionRevision
      this.#virtualizedWidth = width
    }
    const window = this.#virtualizer.window(this.scroller.scrollTop, height)
    const windowKey = `${window.start}:${window.end}:${width}`
    if (!force && windowKey === this.#lastWindow) {
      return
    }
    this.#lastWindow = windowKey
    for (const card of this.mountedCards.values()) {
      this.scroller.remove(card)
      card.destroyRecursively()
    }
    this.mountedCards.clear()
    const toolEntryKeys = projectionEntryKeys(state.transcript, "tool")
    const subagentEntryKeys = projectionEntryKeys(state.transcript, "assistant")
    for (let index = window.start; index < window.end; index += 1) {
      const entry = state.transcript[index]
      if (entry === undefined) {
        continue
      }
      const key = entryKey(entry)
      if (this.mountedCards.has(key)) {
        continue
      }
      const tools = toolEntryKeys.has(key)
        ? Object.values(state.tools).filter((tool) => tool.turnId === entry.agentTurn)
        : []
      const turnSubagents = subagentEntryKeys.has(key)
        ? subagentsForTurn(state, entry.agentTurn)
        : []
      const visibleSubagents = boundedSubagents(turnSubagents)
      const card = new TurnCardRenderable(
        this.ctx,
        this.#theme,
        this.#syntaxStyle,
        entry,
        width,
        formatCost(
          state.turns[entry.agentTurn]?.cost,
          state.turns[entry.agentTurn]?.usage,
        ),
        tools,
        visibleSubagents,
        turnSubagents.length,
        this.#toolExpansion,
        (toolCallId, expanded) => this.#rememberToolExpansion(toolCallId, expanded),
        this.#treeSitterClient,
      )
      this.scroller.insertBefore(card, this.#bottomSpacer)
      this.mountedCards.set(key, card)
    }
    this.#topSpacer.height = window.topSpacer
    this.#bottomSpacer.height = window.bottomSpacer
  }

  #updateTail(state: RottweilerState): void {
    const tail = state.streamingTail
    const allSubagents =
      tail === null
        ? state.subagentOrder
            .map((subagentId) => state.subagents[subagentId])
            .filter(
              (subagent): subagent is SubagentProjection =>
                subagent !== undefined && subagent.status === "running",
            )
        : subagentsForTurn(state, tail.turnId)
    const subagents = boundedSubagents(allSubagents)
    this.subagentPanel.update(subagents, allSubagents.length)
    this.streamingCard.visible = tail !== null || subagents.length > 0
    if (tail === null) {
      this.streamingMarkdown.content = ""
      this.streamingMarkdown.visible = false
      this.#tailHeader.content = "Rottweiler · delegating"
      this.#tailThinking.visible = false
      this.#tailCitations.visible = false
      this.#replaceTailTools([])
      this.streamingCard.height = subagents.length === 0 ? 0 : subagents.length + 5
      return
    }
    const tools = tail.toolCallIds
      .map((toolCallId) => state.tools[toolCallId])
      .filter((tool): tool is ToolProjection => tool !== undefined)
    const activity = tools.some((tool) => tool.status === "awaiting_approval")
      ? "Waiting for approval"
      : tools.some((tool) => tool.status === "running")
        ? "Running tools"
        : tail.thinking.length > 0
          ? "Thinking"
          : "Streaming"
    this.streamingMarkdown.visible = tail.text.length > 0
    this.streamingMarkdown.content = tail.text
    this.streamingMarkdown.streaming = tail.finished === null
    this.#tailHeader.content = `Rottweiler · ${tail.finished === null ? activity : formatCost(tail.finished.cost, tail.finished.usage)}`
    const thinkingRows = tail.thinking.length === 0 ? 0 : Math.min(4, tail.thinking.split("\n").length)
    this.#tailThinking.visible = tail.thinking.length > 0
    this.#tailThinking.content = tail.thinking.length > 0 ? `Thinking · ${tail.thinking}` : ""
    this.#tailThinking.height = thinkingRows
    this.#tailCitations.visible = tail.citations.length > 0
    this.#tailCitations.content = tail.citations
      .map((citation, index) => `[${index + 1}] ${citation.title ?? citation.uri}`)
      .join("  ")
    this.#tailCitations.height = tail.citations.length > 0 ? 1 : 0
    this.#replaceTailTools(tools)
    const textRows = tail.text.length === 0 ? 0 : tail.text.split("\n").length
    this.streamingMarkdown.height = Math.min(20, textRows)
    const toolRows = tools.reduce(
      (rows, tool) => rows + toolBlockRows(tool, this.#toolExpansion),
      0,
    )
    this.#tailTools.height = toolRows
    this.streamingCard.height = Math.min(
      32,
      2 +
        thinkingRows +
        textRows +
        (tail.citations.length > 0 ? 1 : 0) +
        toolRows +
        (subagents.length === 0 ? 0 : subagents.length + 3),
    )
  }

  #replaceTailTools(tools: readonly ToolProjection[]): void {
    for (const child of this.#tailTools.getChildren()) {
      this.#tailTools.remove(child)
      child.destroyRecursively()
    }
    for (const tool of tools) {
      this.#tailTools.add(new ToolBlockRenderable(
        this.ctx,
        this.#theme,
        tool,
        this.#toolExpansion.get(tool.toolCallId),
        (expanded) => this.#rememberToolExpansion(tool.toolCallId, expanded),
      ))
    }
  }

  #rememberToolExpansion(toolCallId: string, expanded: boolean): void {
    if (this.#toolExpansion.get(toolCallId) === expanded) return
    this.#toolExpansion.set(toolCallId, expanded)
    this.#toolExpansionRevision += 1
    const state = this.#state
    if (state === null) return
    this.#updateTail(state)
    this.#reconcile(true)
  }
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
