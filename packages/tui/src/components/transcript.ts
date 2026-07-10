import {
  BoxRenderable,
  MarkdownRenderable,
  ScrollBoxRenderable,
  TextRenderable,
  type RenderContext,
  type SyntaxStyle,
  type TreeSitterClient,
} from "@opentui/core"

import { estimateEntryHeight, entryKey, formatCost, toolOutputText, TranscriptVirtualizer, turnMarkdown } from "../render"
import type { RottweilerState, ToolProjection, TranscriptEntry } from "../state"
import type { RottweilerTheme } from "../theme"

export interface TranscriptRenderableOptions {
  readonly syntaxStyle: SyntaxStyle
  readonly treeSitterClient?: TreeSitterClient
  readonly overscan?: number
}

export class ToolBlockRenderable extends BoxRenderable {
  readonly header: TextRenderable
  readonly body: TextRenderable
  #collapsed: boolean
  #tool: ToolProjection
  #theme: RottweilerTheme

  constructor(ctx: RenderContext, theme: RottweilerTheme, tool: ToolProjection) {
    super(ctx, {
      id: `tool-${tool.toolCallId}`,
      width: "100%",
      minHeight: 2,
      flexDirection: "column",
      border: true,
      borderStyle: "single",
      borderColor: theme.border,
      focusedBorderColor: theme.focus,
      backgroundColor: theme.panelRaised,
      focusable: true,
      paddingX: 1,
      marginTop: 1,
    })
    this.#theme = theme
    this.#tool = tool
    this.#collapsed = tool.status === "finished"
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
    const glyph = tool.status === "running" ? "◌" : tool.isError === true ? "✕" : "✓"
    const approval = tool.status === "awaiting_approval" ? " · approval needed" : ""
    this.header.content = `${this.#collapsed ? "▸" : "▾"} ${glyph} ${tool.name}${approval}`
    this.header.fg =
      tool.status === "awaiting_approval"
        ? this.#theme.warning
        : tool.isError === true
          ? this.#theme.danger
          : tool.status === "finished"
            ? this.#theme.success
            : this.#theme.info
    const live = tool.chunks.map((chunk) => chunk.chunk).join("")
    const final = toolOutputText(tool.output)
    this.body.content = [tool.rationale, live, final].filter(Boolean).join("\n") || "Working…"
    const bodyRows = Math.min(8, Math.max(1, this.body.plainText.split("\n").length))
    this.body.height = bodyRows
    this.height = this.#collapsed ? 2 : bodyRows + 2
  }

  toggle(): void {
    this.#collapsed = !this.#collapsed
    this.body.visible = !this.#collapsed
    this.update(this.#tool)
  }
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
    treeSitterClient?: TreeSitterClient,
  ) {
    const role = entry.turn.role === "assistant" ? "Rottweiler" : entry.turn.role
    super(ctx, {
      id: `turn-${entryKey(entry)}`,
      width: "100%",
      height:
        estimateEntryHeight(entry, width) + 1 +
        tools.reduce((rows, tool) => rows + (tool.status === "finished" ? 2 : 4), 0),
      flexDirection: "column",
      flexShrink: 0,
      border: false,
      backgroundColor: entry.turn.role === "user" ? theme.panelRaised : theme.background,
      paddingX: 1,
      paddingY: 1,
    })
    this.header = new TextRenderable(ctx, {
      content: `${role} · ${cost}`,
      fg: entry.turn.role === "assistant" ? theme.accentStrong : theme.info,
      height: 1,
      flexShrink: 0,
    })
    this.markdown = new MarkdownRenderable(ctx, {
      id: `markdown-${entryKey(entry)}`,
      content: turnMarkdown(entry.turn),
      syntaxStyle,
      ...(treeSitterClient === undefined ? {} : { treeSitterClient }),
      fg: theme.foreground,
      conceal: true,
      concealCode: false,
      streaming: false,
      width: "100%",
      flexGrow: 1,
      internalBlockMode: "top-level",
      tableOptions: { style: "columns", widthMode: "full", wrapMode: "word" },
    })
    this.add(this.header)
    this.add(this.markdown)
    for (const tool of tools) {
      this.add(new ToolBlockRenderable(ctx, theme, tool))
    }
  }
}

export class TranscriptRenderable extends BoxRenderable {
  readonly scroller: ScrollBoxRenderable
  readonly streamingCard: BoxRenderable
  readonly streamingMarkdown: MarkdownRenderable
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
  #state: RottweilerState | null = null
  #transcript: readonly TranscriptEntry[] | null = null
  #virtualizedTranscript: readonly TranscriptEntry[] | null = null
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
      minHeight: 5,
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
    this.streamingCard.add(this.#tailHeader)
    this.streamingCard.add(this.#tailThinking)
    this.streamingCard.add(this.streamingMarkdown)
    this.streamingCard.add(this.#tailCitations)
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
    this.#transcript = state.transcript
    this.#updateTail(state)
    this.#reconcile(transcriptChanged)
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
    if (this.#virtualizedTranscript !== state.transcript || this.#virtualizedWidth !== width) {
      this.#virtualizer.update(state.transcript, width)
      this.#virtualizedTranscript = state.transcript
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
    for (let index = window.start; index < window.end; index += 1) {
      const entry = state.transcript[index]
      if (entry === undefined) {
        continue
      }
      const key = entryKey(entry)
      if (this.mountedCards.has(key)) {
        continue
      }
      const tools = Object.values(state.tools).filter((tool) => tool.turnId === entry.agentTurn)
      const card = new TurnCardRenderable(
        this.ctx,
        this.#theme,
        this.#syntaxStyle,
        entry,
        width,
        formatCost(state.turns[entry.agentTurn]?.cost),
        tools,
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
    this.streamingCard.visible = tail !== null
    if (tail === null) {
      this.streamingMarkdown.content = ""
      this.#replaceTailTools([])
      return
    }
    this.streamingMarkdown.content = tail.text.length === 0 ? "_Waiting for response…_" : tail.text
    this.streamingMarkdown.streaming = tail.finished === null
    this.#tailHeader.content = `Rottweiler · ${tail.finished === null ? "streaming" : formatCost(tail.finished.cost)}`
    const thinkingRows = tail.thinking.length === 0 ? 0 : Math.min(4, tail.thinking.split("\n").length)
    this.#tailThinking.visible = tail.thinking.length > 0
    this.#tailThinking.content = tail.thinking.length > 0 ? `Thinking · ${tail.thinking}` : ""
    this.#tailThinking.height = thinkingRows
    this.#tailCitations.visible = tail.citations.length > 0
    this.#tailCitations.content = tail.citations
      .map((citation, index) => `[${index + 1}] ${citation.title ?? citation.uri}`)
      .join("  ")
    this.#tailCitations.height = tail.citations.length > 0 ? 1 : 0
    const tools = tail.toolCallIds
      .map((toolCallId) => state.tools[toolCallId])
      .filter((tool): tool is ToolProjection => tool !== undefined)
    this.#replaceTailTools(tools)
    const textRows = Math.max(1, tail.text.split("\n").length)
    this.streamingMarkdown.height = Math.min(20, textRows)
    const toolRows = tools.reduce(
      (rows, tool) => rows + (tool.status === "finished" ? 2 : 4),
      0,
    )
    this.#tailTools.height = toolRows
    this.streamingCard.height = Math.min(
      32,
      2 + thinkingRows + textRows + (tail.citations.length > 0 ? 1 : 0) + toolRows,
    )
  }

  #replaceTailTools(tools: readonly ToolProjection[]): void {
    for (const child of this.#tailTools.getChildren()) {
      this.#tailTools.remove(child)
      child.destroyRecursively()
    }
    for (const tool of tools) {
      this.#tailTools.add(new ToolBlockRenderable(this.ctx, this.#theme, tool))
    }
  }
}
