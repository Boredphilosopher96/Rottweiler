export { ToolBlockRenderable, boundedToolBody, toolOutputContent, toolOutputPreview, toolRowPreview, type ToolBlockRendering } from "./tool-block"
import { TextRenderable } from "../text"
import type { ClientDiagnostics } from "../../client-diagnostics"
import { bindSelectableClick } from "../selectable-click"
import type { HistoryAnchor } from "../../history/controller"
import type { TranscriptContent } from "../../protocol"
import {
  BoxRenderable,
  fg,
  t,
  type RenderContext,
  type SyntaxStyle,
  type TreeSitterClient,
 } from "@opentui/core"

import { truncateToCells } from "../../render"
import type { SubagentProjection } from "../../state"
import type { RottweilerTheme } from "../../theme"

export interface TranscriptRenderableOptions {
  readonly diagnostics?: ClientDiagnostics | undefined
  readonly syntaxStyle: SyntaxStyle
  readonly treeSitterClient?: TreeSitterClient
  readonly onInteraction?: () => void
  readonly onOpenSubagent?: (subagentId: string) => void
  readonly onOpenToolOutput?: (invocationId: string) => void
  readonly onOpenLiveContent?: (source: import("../../protocol").TranscriptContentSource) => void
  readonly onOpenContent?: (source: import("../../protocol").TranscriptContentSource) => void
  readonly onOpenChild?: (child: Extract<TranscriptContent, { type: "subagent" }>) => void
  /** Agent definition name of a child, when the session's catalog knows it. */
  readonly childAgentName?: (subagentId: string) => string | null
  readonly onHistoryAnchor?: (anchor: HistoryAnchor) => void
  readonly onHistorySeek?: (ordinal: bigint) => void
  readonly onHistorySearch?: (source: import("../../protocol").SessionSearchMatch) => Promise<void>
  readonly onHistoryAround?: (item: string) => void | Promise<void>
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
