import type { HistorySnapshot } from "../history/controller"
import { TranscriptRowRenderable } from "./transcript/row"
import { TranscriptScrollWindow } from "./transcript/scroll-window"
import {
  ReasoningBlockRenderable,
  ToolBlockRenderable,
  type TranscriptRenderableOptions,
  presentableReasoning,
 } from "./transcript/blocks"
import { DISPLAY_TRUNCATION_MARKER } from "../state/display-buffer"
import type { TranscriptClientState } from "../recycle-state"
import {
  type BaseRenderable,
  BoxRenderable,
  MarkdownRenderable,
  ScrollBarRenderable,
  TextRenderable,
  bold,
  fg,
  t,
  type RenderContext,
  type SyntaxStyle,
  type TreeSitterClient,
 } from "@opentui/core"

import { formatCost, getScrollAcceleration, terminalMarkdown, truncateToCells } from "../render"
import type { RottweilerState, ToolProjection } from "../state"
import type { RottweilerTheme } from "../theme"

// OpenTUI viewport culling skips paint work but retains every mounted renderable.
// Bound the expensive live card tree independently of the reducer's retained
// recent history. Long sessions otherwise grow by roughly one pair of Markdown
// renderables per turn even after context compaction.
const MAX_MOUNTED_TRANSCRIPT_ENTRIES = 16

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

export class TranscriptRenderable extends BoxRenderable {
  readonly scroller: TranscriptScrollWindow
  readonly emptyState: BoxRenderable
  readonly emptyStateTitle: TextRenderable
  readonly emptyStateHint: TextRenderable
  readonly streamingCard: BoxRenderable
  readonly streamingMarkdown: MarkdownRenderable
  readonly compactionCard: BoxRenderable
  readonly compactionMarkdown: MarkdownRenderable
  readonly mountedCards = new Map<string, TranscriptRowRenderable>()
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
  readonly #onOpenToolOutput: ((toolCallId: string) => void) | undefined
  readonly #toolExpansion = new Map<string, boolean>()
  readonly #tailToolCards = new Map<string, ToolBlockRenderable>()
  readonly #tailToolPool: ToolBlockRenderable[] = []
  readonly #reasoningExpansion = new Map<string, boolean>()
  #selectedBlockId: string | null = null
  #state: RottweilerState | null = null
  readonly #historyOptions: TranscriptRenderableOptions
  readonly #ordinalBar: ScrollBarRenderable
  #history: HistorySnapshot | null = null
  readonly #finalHistoryInvocations = new Set<string>()
  #windowStart = 0
  #syncingOrdinal = false
  #requestedAnchor: { readonly id: string; readonly offset: number } | null = null
  #pendingBottom = false
  #settledAnchor: { readonly id: string; readonly offset: number } | null = null
  #pendingAnchor: { readonly id: string; readonly offset: number } | null = null
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
      flexDirection: "row",
      minWidth: 1,
      width: "100%",
      flexGrow: 1,
      minHeight: 1,
      backgroundColor: theme.background,
    })
    this.#theme = theme
    this.#historyOptions = options
    this.#syntaxStyle = options.syntaxStyle
    this.#treeSitterClient = options.treeSitterClient
    this.#onInteraction = options.onInteraction
    this.#onOpenToolOutput = options.onOpenToolOutput
    this.scroller = new TranscriptScrollWindow(ctx, {
      id: "transcript-scroll",
      flexGrow: 1,
      flexShrink: 1,
      minWidth: 1,
      width: "100%",
      height: "100%",
      scrollY: true,
      scrollX: false,
      stickyScroll: true,
      stickyStart: "bottom",
      scrollAcceleration: getScrollAcceleration(),
      viewportCulling: true,
      contentOptions: { flexDirection: "column", width: "100%" },
      verticalScrollbarOptions: { visible: false, showArrows: false },
    })
    this.scroller.diagnostics = options.diagnostics
    this.scroller.afterLayout = () => {
      if (this.#restoreAnchor()) return
      this.#settledAnchor = this.#captureAnchor()
      if (this.#settledAnchor !== null) this.#historyOptions.onHistoryAnchor?.(this.#settledAnchor)
    }
    this.scroller.onMouseScroll = event => {
      const direction = event.scroll?.direction
      if (direction !== "up" && direction !== "down") return
      this.scrollBy(direction === "up" ? -1 : 1, "step")
      event.preventDefault()
      event.stopPropagation()
    }
    this.#ordinalBar = new ScrollBarRenderable(ctx, {
      id: "history-ordinal-scroll", orientation: "vertical", width: 1, height: "100%", showArrows: false,
      onChange: position => {
        if (this.#syncingOrdinal || this.#history === null) return
        const total = this.#history.total
        const scale = Math.min(1_000_000, Number(total < 1_000_000n ? total : 1_000_000n))
        if (scale > 0) this.#historyOptions.onHistorySeek?.(BigInt(Math.floor(position)) * total / BigInt(scale))
      },
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
    this.add(this.#ordinalBar)
  }

  get mountedEntryCount(): number {
    return this.mountedCards.size
  }

  get mountedKeys(): readonly string[] {
    return [...this.mountedCards.values()].sort((left, right) =>
      BigInt(left.item.ordinal) < BigInt(right.item.ordinal) ? -1 : 1).map(card => card.item.id)
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

  setHistory(history: HistorySnapshot): void {
    const startedAt = this.#historyOptions.diagnostics?.start()
    try {
      const sessionChanged = this.#history?.sessionId !== history.sessionId
      if (sessionChanged) {
        for (const card of this.mountedCards.values()) {
          this.scroller.remove(card)
          card.destroyRecursively()
        }
        this.mountedCards.clear()
        this.#selectedBlockId = null
        this.#toolExpansion.clear()
        this.#reasoningExpansion.clear()
        this.#requestedAnchor = null
        this.#pendingAnchor = null
        this.#settledAnchor = null
      }
      const selected = history.selection !== null && history.selection !== this.#history?.selection
      const changed = this.#history?.page !== history.page || selected
      const anchor = this.#requestedAnchor ?? this.#captureAnchor() ?? history.anchor
      this.#history = history
      if (changed) {
        this.#finalHistoryInvocations.clear()
        for (const item of history.page?.items ?? []) {
          if (item.content.type === "tool" && item.content.status.type === "finished") {
            this.#finalHistoryInvocations.add(item.content.invocation_id)
          }
        }
        const items = history.page?.items ?? []
        this.#requestedAnchor = null
        const previousIndex = anchor === null ? -1 : items.findIndex(item => item.id === anchor.id)
        const selectionIndex = history.selection === null ? -1 : items.findIndex(item => BigInt(item.ordinal) === history.selection?.ordinal)
        const replacement = history.page?.anchor.type === "replaced" ? history.page.anchor.replacement : null
        const replacementIndex = replacement === null ? -1 : items.findIndex(item => item.id === replacement)
        const target = selectionIndex >= 0 ? selectionIndex : previousIndex >= 0 ? previousIndex : replacementIndex
        this.#windowStart = history.following ? Math.max(0, items.length - MAX_MOUNTED_TRANSCRIPT_ENTRIES)
          : Math.max(0, Math.min(items.length - MAX_MOUNTED_TRANSCRIPT_ENTRIES, target < 0 ? 0 : target - 4))
        const selectedItem = items[selectionIndex]
        this.#pendingAnchor = history.following ? null : selectedItem !== undefined
          ? { id: selectedItem.id, offset: 0 } : previousIndex >= 0 ? anchor
            : replacement === null ? null : { id: replacement, offset: 0 }
        this.#pendingBottom = history.following
        this.#reconcileHistory()
        if (history.following) this.scroller.scrollTo(this.scroller.scrollHeight)
      }
      this.#syncOrdinalBar()
      if (this.#state !== null) {
        this.#updateEmptyState(this.#state)
        this.#updateTail(this.#state)
        this.#syncBlockSelection()
      }
      this.requestRender()
    } finally { if (startedAt !== undefined) this.#historyOptions.diagnostics?.finish("history_update", startedAt) }
  }

  update(state: RottweilerState, agentName = "Rottweiler"): void {
    this.#state = state
    this.#agentName = truncateToCells(agentName.replace(/\s+/g, " ").trim(), 48) || "Child agent"
    this.#updateEmptyState(state)
    this.#updateTail(state)
    this.#updateCompaction(state)
    this.#syncBlockSelection()
  }

  #updateEmptyState(state: RottweilerState): void {
    this.emptyState.visible = !state.replay.active && this.mountedCards.size === 0
      && state.streamingTail === null && !state.compaction.active
    if (this.emptyState.visible) {
      this.emptyStateHint.content = this.#history?.error ?? (this.#history?.loading
        ? "Loading transcript…" : "Describe a task, or press / for commands.")
    }
  }

  setScrollOffset(scrollTop: number): void {
    this.#pendingBottom = false
    this.#pendingAnchor = null
    this.scroller.scrollTo(scrollTop)
  }

  scrollBy(direction: 1 | -1, unit: "step" | "viewport"): void {
    this.#historyOptions.onHistoryFollowing?.(false)
    this.#moveWindow(direction)
    this.scroller.scrollBy(direction, unit)
  }

  scrollTo(position: number): void {
    this.#historyOptions.onHistoryBoundary?.(position <= 0 ? "first" : "latest")
    this.scroller.scrollTo(position)
  }

  protected override onResize(_width: number, _height: number): void {
    if (this.#history?.following === false && this.#pendingAnchor === null) this.#pendingAnchor = this.#settledAnchor
    if (this.#state !== null) {
      this.#updateTail(this.#state)
      this.#updateCompaction(this.#state)
    }
    this.#reconcileHistory()
  }

  #reconcileHistory(): void {
    const items = this.#history?.page?.items.slice(this.#windowStart, this.#windowStart + MAX_MOUNTED_TRANSCRIPT_ENTRIES) ?? []
    const desired = new Set(items.map(item => item.id))
    for (const [key, card] of this.mountedCards) {
      if (desired.has(key)) continue
      this.scroller.remove(card)
      this.mountedCards.delete(key)
      card.destroyRecursively()
      this.#recycledSinceCollection++
    }
    let reference: BoxRenderable = this.streamingCard
    for (const item of [...items].reverse()) {
      let card = this.mountedCards.get(item.id)
      if (card === undefined) {
        if (item.agent_turn === this.#tailReasoningTurnId && item.content.type === "conversation"
          && item.content.blocks.some(block => block.type === "reasoning")
          && this.#selectedBlockId === this.#tailReasoning.blockId) {
          this.#selectedBlockId = `history-reasoning:${item.id}`
        }
        card = new TranscriptRowRenderable(this.ctx, this.#theme, item, {
          ...this.#historyOptions,
          onExpansionChange: (id, expanded) => rememberExpansion(this.#toolExpansion, id, expanded),
          onReasoningExpansion: (id, expanded) => rememberExpansion(this.#reasoningExpansion, id, expanded),
          reasoningExpanded: this.#reasoningExpansion.get(item.id)
            ?? (item.agent_turn === this.#tailReasoningTurnId ? this.#tailReasoning.expanded : true),
        }, this.#toolExpansion.get(item.content.type === "tool" ? item.content.invocation_id : `history:${item.id}`))
        this.mountedCards.set(item.id, card)
      }
      card.update(item, Math.max(20, this.width || this.ctx.width))
      this.scroller.insertBefore(card, reference)
      reference = card
    }
    if (this.#recycledSinceCollection >= 64) { Bun.gc(true); this.#recycledSinceCollection = 0 }
    this.#syncBlockSelection()
  }

  #captureAnchor(): { readonly id: string; readonly offset: number } | null {
    const top = this.scroller.viewport.y
    const bottom = top + this.scroller.viewport.height
    let visible: TranscriptRowRenderable | undefined
    for (const card of this.mountedCards.values()) {
      if (card.visible && card.y + card.height > top && card.y < bottom
        && (visible === undefined || card.y < visible.y)) visible = card
    }
    return visible === undefined ? null : { id: visible.item.id, offset: visible.y - top }
  }

  #restoreAnchor(): boolean {
    if (this.#pendingBottom) {
      this.#pendingBottom = false
      const previous = this.scroller.scrollTop
      this.scroller.scrollTo(this.scroller.scrollHeight)
      if (this.scroller.scrollTop !== previous) return true
    }
    const anchor = this.#pendingAnchor
    if (anchor === null) return false
    const card = this.mountedCards.get(anchor.id)
    if (card !== undefined) {
      const previous = this.scroller.scrollTop
      this.scroller.scrollTo(previous + card.y - this.scroller.viewport.y - anchor.offset)
      if (this.scroller.scrollTop !== previous) return true
    }
    this.#pendingAnchor = null
    return false
  }

  #moveWindow(direction: -1 | 1): void {
    const history = this.#history
    const items = history?.page?.items
    if (history === null || items === undefined || history.loading) return
    const atEdge = direction < 0 ? this.scroller.scrollTop <= 3
      : this.scroller.scrollTop + this.scroller.viewport.height >= this.scroller.scrollHeight - 3
    if (!atEdge) return
    const anchor = this.#captureAnchor()
    const next = Math.max(0, Math.min(Math.max(0, items.length - MAX_MOUNTED_TRANSCRIPT_ENTRIES), this.#windowStart + direction * 4))
    if (next !== this.#windowStart) {
      this.#windowStart = next
      this.#pendingAnchor = anchor
      this.#reconcileHistory()
    } else {
      const boundary = direction < 0 ? items[0] : items.at(-1)
      if (boundary !== undefined && (direction < 0 ? BigInt(boundary.ordinal) > 0n : BigInt(boundary.ordinal) + 1n < history.total)) {
        this.#requestedAnchor = anchor
        this.#historyOptions.onHistoryAround?.(boundary.id)
      } else if (direction > 0) this.#historyOptions.onHistoryFollowing?.(true)
    }
    this.#syncOrdinalBar()
  }

  #syncOrdinalBar(): void {
    const total = this.#history?.total ?? 0n
    this.#syncingOrdinal = true
    const scale = total < 1_000_000n ? total : 1_000_000n
    this.#ordinalBar.scrollSize = Number(scale)
    this.#ordinalBar.viewportSize = total === 0n ? 0 : Math.max(1, Number(BigInt(this.mountedCards.size) * scale / total))
    const ordinal = this.#history?.page?.items[this.#windowStart]?.ordinal ?? "0"
    this.#ordinalBar.scrollPosition = total === 0n ? 0 : Number(BigInt(ordinal) * scale / total)
    this.#ordinalBar.visible = total > BigInt(this.mountedCards.size)
    this.#syncingOrdinal = false
  }

  #updateTail(state: RottweilerState): void {
    const tail = state.streamingTail
    const tools = (tail?.toolCallIds ?? [])
      .map(toolCallId => state.tools[toolCallId])
      .filter((tool): tool is ToolProjection => tool !== undefined
        && !this.#finalHistoryInvocations.has(tool.invocationId))
    const liveInvocations = new Set(tools.map(tool => tool.invocationId))
    for (const card of this.mountedCards.values()) {
      const content = card.item.content
      card.visible = !(content.type === "tool" && content.status.type === "running"
        && liveInvocations.has(content.invocation_id))
    }
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
    const retained = new Set(tools.map((tool) => tool.invocationId))
    for (const [toolCallId, card] of this.#tailToolCards) {
      if (retained.has(toolCallId)) continue
      this.#tailTools.remove(card)
      this.#tailToolCards.delete(toolCallId)
      this.#tailToolPool.push(card)
    }
    for (const tool of tools) {
      let card = this.#tailToolCards.get(tool.invocationId)
      if (card === undefined) {
        card = this.#tailToolPool.pop()
        if (card === undefined) {
          card = new ToolBlockRenderable(
            this.ctx,
            this.#theme,
            tool,
            this.#toolExpansion.get(tool.invocationId),
            (expanded) => this.#rememberToolExpansion(tool.invocationId, expanded),
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
            this.#toolExpansion.get(tool.invocationId),
            (expanded) => this.#rememberToolExpansion(tool.invocationId, expanded),
          )
        }
        card.update(tool, Math.max(20, this.width || this.ctx.width), this.#state?.workspaceRoots?.generation ?? "")
        this.#tailToolCards.set(tool.invocationId, card)
        this.#tailTools.add(card)
      } else {
        card.update(tool, Math.max(20, this.width || this.ctx.width), this.#state?.workspaceRoots?.generation ?? "")
      }
    }
    while (this.#tailToolPool.length > 16) this.#tailToolPool.shift()?.destroyRecursively()
  }

  override destroy(): void {
    for (const card of this.#tailToolPool) card.destroyRecursively()
    this.#tailToolPool.length = 0
    super.destroy()
  }

  #rememberToolExpansion(toolCallId: string, expanded: boolean): void {
    if (this.#toolExpansion.get(toolCallId) === expanded) return
    rememberExpansion(this.#toolExpansion, toolCallId, expanded)
    const state = this.#state
    if (state === null) return
    this.#updateTail(state)
  }

  #blocksInVisualOrder(): Array<ToolBlockRenderable | ReasoningBlockRenderable | TranscriptRowRenderable> {
    const blocks: Array<ToolBlockRenderable | ReasoningBlockRenderable | TranscriptRowRenderable> = []
    const visit = (renderable: BaseRenderable): void => {
      if (!renderable.visible) return
      if (
        renderable instanceof ToolBlockRenderable ||
        renderable instanceof ReasoningBlockRenderable
        || (renderable instanceof TranscriptRowRenderable && renderable.item.content.type === "tool")
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

function turnDetail(
  cost: Parameters<typeof formatCost>[0],
  usage: Parameters<typeof formatCost>[1],
): string {
  const detail = formatCost(cost, usage)
  return cost?.kind === "subscription_quota" && (cost.used === null || cost.used === undefined)
    ? `turn usage · ${detail}`
    : detail
}


function rememberExpansion(values: Map<string, boolean>, key: string, expanded: boolean): void {
  values.delete(key)
  values.set(key, expanded)
  if (values.size > 256) {
    const oldest = values.keys().next().value
    if (oldest !== undefined) values.delete(oldest)
  }
}
