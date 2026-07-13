import {
  BoxRenderable,
  MarkdownRenderable,
  ScrollBoxRenderable,
  TextRenderable,
  type RenderContext,
  type SyntaxStyle,
  type TreeSitterClient,
} from "@opentui/core"

import {
  formatCost,
  formatToolArguments,
  getScrollAcceleration,
  terminalMarkdown,
  toolOutputText,
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
}

const MAX_VISIBLE_SUBAGENTS = 8

function entryKey(entry: TranscriptEntry): string {
  return `${entry.sequenceId}:${entry.agentTurn}:${entry.turn.role}`
}

export class ReasoningBlockRenderable extends BoxRenderable {
  readonly header: TextRenderable
  readonly body: MarkdownRenderable
  #content = ""
  #expanded = false
  #streaming = false
  #width = 80
  readonly #onExpansionChange: (expanded: boolean) => void
  readonly #onInteraction: (() => void) | undefined

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    syntaxStyle: SyntaxStyle,
    options: {
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
      width: "100%",
      flexDirection: "column",
      flexShrink: 0,
      backgroundColor: theme.background,
      focusable: false,
    })
    this.#expanded = options.expanded ?? false
    this.#streaming = options.streaming ?? false
    this.#width = options.width
    this.#onExpansionChange = options.onExpansionChange
    this.#onInteraction = options.onInteraction
    this.header = new TextRenderable(ctx, {
      content: "",
      fg: theme.warning,
      height: 1,
      flexShrink: 0,
      wrapMode: "none",
    })
    this.body = new MarkdownRenderable(ctx, {
      content: "",
      syntaxStyle,
      ...(options.treeSitterClient === undefined ? {} : { treeSitterClient: options.treeSitterClient }),
      fg: colorWithOpacity(theme.markdownText, theme.thinkingOpacity),
      conceal: true,
      concealCode: false,
      streaming: this.#streaming,
      width: "100%",
      visible: this.#expanded,
      internalBlockMode: "top-level",
    })
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

  update(content: string, streaming = this.#streaming, width = this.#width): void {
    this.#content = presentableReasoning(content)
    this.#streaming = streaming
    this.#width = width
    this.visible = this.#content !== ""
    this.body.streaming = streaming
    this.#layout()
  }

  collapse(notify = true): void {
    if (!this.#expanded) return
    this.#expanded = false
    this.#layout()
    if (notify) this.#onExpansionChange(false)
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
    const state = this.#streaming ? "Thinking" : "Thought"
    const indicator = this.#streaming && !this.#expanded ? "◌" : this.#expanded ? "⌄" : "›"
    this.header.content = `${indicator} ${state}: ${reasoningTitle(this.#content)}`
    this.body.visible = this.#expanded
    this.body.content = this.#expanded ? this.#content : ""
  }
}

function presentableReasoning(content: string): string {
  return content.replaceAll("[REDACTED]", "").trim()
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
  return singleLine(first, 72)
}

function colorWithOpacity(color: string, opacity: number): string {
  const match = /^#([0-9A-Fa-f]{6})([0-9A-Fa-f]{2})?$/.exec(color)
  if (match === null) return color
  const sourceAlpha = match[2] === undefined ? 255 : Number.parseInt(match[2], 16)
  const alpha = Math.round(sourceAlpha * Math.max(0, Math.min(1, opacity)))
  return `#${match[1]}${alpha.toString(16).padStart(2, "0")}`
}

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
      // Expansion is mouse-driven while keyboard focus remains owned by the
      // transcript scroller/composer. Individual tool rows must not trap it.
      focusable: false,
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
  readonly reasoning: ReasoningBlockRenderable | null

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    syntaxStyle: SyntaxStyle,
    entry: TranscriptEntry,
    width: number,
    detail: string | null,
    tools: readonly ToolProjection[],
    subagents: readonly SubagentProjection[],
    subagentTotal: number,
    toolExpansion: Map<string, boolean>,
    reasoningExpanded: boolean,
    onToolExpansionChange: (toolCallId: string, expanded: boolean) => void,
    onReasoningExpansionChange: (expanded: boolean) => void,
    onInteraction: (() => void) | undefined,
    treeSitterClient?: TreeSitterClient,
  ) {
    const markdown = terminalMarkdown(turnMarkdown(entry.turn), Math.max(20, width - 4))
    const reasoning = turnReasoningMarkdown(entry.turn)
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
      width,
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
        : `${role}${detail === null ? "" : ` · ${detail}`}`,
      fg: entry.turn.role === "assistant" ? theme.accentStrong : theme.info,
      height: toolOnly ? 0 : 1,
      flexShrink: 0,
      visible: !toolOnly && markdown !== "",
    })
    this.markdown = new MarkdownRenderable(ctx, {
      id: `markdown-${entryKey(entry)}`,
      content: markdown,
      syntaxStyle,
      ...(treeSitterClient === undefined ? {} : { treeSitterClient }),
      fg: theme.markdownText,
      conceal: true,
      concealCode: false,
      streaming: false,
      width: Math.max(1, width - 2),
      flexShrink: 0,
      visible: !toolOnly,
      internalBlockMode: "top-level",
      tableOptions: { style: "grid", widthMode: "full", wrapMode: "word" },
    })
    this.reasoning = reasoning === ""
      ? null
      : new ReasoningBlockRenderable(ctx, theme, syntaxStyle, {
          content: reasoning,
          expanded: reasoningExpanded,
          streaming: false,
          width,
          ...(treeSitterClient === undefined ? {} : { treeSitterClient }),
          onExpansionChange: onReasoningExpansionChange,
          ...(onInteraction === undefined ? {} : { onInteraction }),
        })
    // Selection can focus a retained transcript node. The app restores its
    // configured keyboard-input target after the pointer interaction ends.
    this.onMouseUp = () => onInteraction?.()
    if (!toolOnly) {
      this.add(this.header)
      if (this.reasoning !== null) this.add(this.reasoning)
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
  readonly #tailReasoning: ReasoningBlockRenderable
  readonly #tailHeader: TextRenderable
  readonly #tailCitations: TextRenderable
  readonly #tailTools: BoxRenderable
  readonly #theme: RottweilerTheme
  readonly #syntaxStyle: SyntaxStyle
  readonly #treeSitterClient: TreeSitterClient | undefined
  readonly #onInteraction: (() => void) | undefined
  readonly #toolExpansion = new Map<string, boolean>()
  readonly #reasoningExpansion = new Map<string, boolean>()
  readonly #cardSignatures = new Map<string, string>()
  #state: RottweilerState | null = null
  #transcript: readonly TranscriptEntry[] | null = null
  #presentableTranscript: readonly TranscriptEntry[] = []
  #tools: RottweilerState["tools"] | null = null
  #turns: RottweilerState["turns"] | null = null
  #subagents: RottweilerState["subagents"] | null = null
  #tailToolsSignature = ""
  #tailReasoningTurnId: string | null = null

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
      verticalScrollbarOptions: { showArrows: false, trackOptions: { backgroundColor: theme.panel } },
    })
    this.scroller.onMouseUp = () => this.#onInteraction?.()
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
    this.#tailReasoning = new ReasoningBlockRenderable(ctx, theme, options.syntaxStyle, {
      content: "",
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
      flexShrink: 0,
      // OpenTUI's incremental parser can retain every completed top-level
      // block and only reconcile the unstable trailing block. Keeping one
      // persistent MarkdownRenderable in this mode prevents the raw-markdown
      // flash and full document relayout that otherwise occurs on each token.
      internalBlockMode: "top-level",
      tableOptions: { style: "grid", widthMode: "full", wrapMode: "word" },
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
    this.streamingCard.add(this.#tailReasoning)
    this.streamingCard.add(this.streamingMarkdown)
    this.streamingCard.add(this.#tailCitations)
    this.streamingCard.add(this.subagentPanel)
    this.streamingCard.add(this.#tailTools)
    this.scroller.add(this.streamingCard)
    this.add(this.scroller)
  }

  get mountedEntryCount(): number {
    return this.mountedCards.size
  }

  get mountedKeys(): readonly string[] {
    return this.#presentableTranscript.map(entryKey)
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
    if (transcriptChanged || cardProjectionChanged) {
      this.#presentableTranscript = presentableTranscript(state)
    }
    this.#updateTail(state)
    if (transcriptChanged || cardProjectionChanged || turnProjectionChanged) {
      this.#reconcileHistory()
    }
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
    this.#reconcileHistory()
  }

  #reconcileHistory(): void {
    const state = this.#state
    if (state === null) return
    const width = Math.max(20, this.width || this.ctx.width)
    const transcript = this.#presentableTranscript
    const desiredKeys = new Set(transcript.map(entryKey))
    for (const [key, card] of this.mountedCards) {
      if (desiredKeys.has(key)) continue
      this.scroller.remove(card)
      card.destroyRecursively()
      this.mountedCards.delete(key)
      this.#cardSignatures.delete(key)
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
      const tools = toolEntryKeys.has(key)
        ? Object.values(state.tools).filter((tool) => tool.turnId === entry.agentTurn)
        : []
      const turnSubagents = subagentEntryKeys.has(key)
        ? subagentsForTurn(state, entry.agentTurn)
        : []
      const visibleSubagents = boundedSubagents(turnSubagents)
      const detail = entry.turn.role === "assistant" &&
          lastAssistantEntryByTurn.get(entry.agentTurn) === key &&
          state.turns[entry.agentTurn]?.cost != null
        ? turnDetail(state.turns[entry.agentTurn]?.cost, state.turns[entry.agentTurn]?.usage)
        : null
      const signature = JSON.stringify([
        width,
        entry,
        detail,
        tools,
        visibleSubagents,
        turnSubagents.length,
        tools.map((tool) => this.#toolExpansion.get(tool.toolCallId) === true),
        this.#reasoningExpansion.get(key) === true,
      ])
      const retained = this.mountedCards.get(key)
      if (retained !== undefined && this.#cardSignatures.get(key) === signature) {
        reference = retained
        continue
      }
      if (retained !== undefined) {
        this.scroller.remove(retained)
        retained.destroyRecursively()
      }
      const card = new TurnCardRenderable(
        this.ctx,
        this.#theme,
        this.#syntaxStyle,
        entry,
        width,
        detail,
        tools,
        visibleSubagents,
        turnSubagents.length,
        this.#toolExpansion,
        this.#reasoningExpansion.get(key) ?? false,
        (toolCallId, expanded) => this.#rememberToolExpansion(toolCallId, expanded),
        (expanded) => this.#rememberReasoningExpansion(key, expanded),
        this.#onInteraction,
        this.#treeSitterClient,
      )
      this.scroller.insertBefore(card, reference)
      this.mountedCards.set(key, card)
      this.#cardSignatures.set(key, signature)
      reference = card
    }
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
      // Finalize the trailing parser block before clearing it. This mirrors
      // OpenCode's streaming lifecycle and avoids leaving an unfinished code
      // fence/table parse state behind when the next response starts.
      this.streamingMarkdown.streaming = false
      this.streamingMarkdown.content = ""
      this.streamingMarkdown.visible = false
      this.#tailHeader.content = "Rottweiler · delegating"
      this.#tailReasoning.update("", false, Math.max(20, this.width || this.ctx.width))
      this.#tailReasoningTurnId = null
      this.#tailCitations.visible = false
      this.#replaceTailTools([])
      return
    }
    const tools = tail.toolCallIds
      .map((toolCallId) => state.tools[toolCallId])
      .filter((tool): tool is ToolProjection => tool !== undefined)
    const reasoning = presentableReasoning(tail.thinking)
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
      tail.text,
      Math.max(20, (this.width || this.ctx.width) - 4),
      tail.finished === null ? "streaming" : "complete",
    )
    this.#tailHeader.content = `Rottweiler · ${tail.finished === null ? activity : turnDetail(tail.finished.cost, tail.finished.usage)}`
    if (this.#tailReasoningTurnId !== tail.turnId) {
      this.#tailReasoning.collapse(false)
      this.#tailReasoningTurnId = tail.turnId
    }
    this.#tailReasoning.update(
      reasoning,
      tail.finished === null,
      Math.max(20, this.width || this.ctx.width),
    )
    this.#tailCitations.visible = tail.citations.length > 0
    this.#tailCitations.content = tail.citations
      .map((citation, index) => `[${index + 1}] ${citation.title ?? citation.uri}`)
      .join("  ")
    this.#tailCitations.height = tail.citations.length > 0 ? 1 : 0
    this.#replaceTailTools(tools)
  }

  #replaceTailTools(tools: readonly ToolProjection[]): void {
    const signature = tools.map((tool) => [
      tool.toolCallId,
      tool.status,
      tool.name,
      JSON.stringify(tool.args),
      tool.chunks.map((chunk) => `${chunk.stream}:${chunk.chunk}`).join(""),
      JSON.stringify(tool.output),
      tool.isError,
      this.#toolExpansion.get(tool.toolCallId) === true,
    ].join("\u0000")).join("\u0001")
    if (signature === this.#tailToolsSignature) return
    this.#tailToolsSignature = signature
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
    const state = this.#state
    if (state === null) return
    this.#updateTail(state)
  }

  #rememberReasoningExpansion(key: string, expanded: boolean): void {
    if ((this.#reasoningExpansion.get(key) ?? false) === expanded) return
    this.#reasoningExpansion.set(key, expanded)
  }
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
