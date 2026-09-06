import { TextRenderable } from "../text"
import { bindSelectableClick } from "../selectable-click"
import { BoxRenderable, CodeRenderable, MarkdownRenderable, t, fg, bold, type RenderContext, type SyntaxStyle, type TreeSitterClient } from "@opentui/core"
import type { TranscriptBodyPreview, TranscriptContent, TranscriptContentSource, TranscriptItem } from "../../protocol"
import type { RottweilerTheme } from "../../theme"
import { ReasoningBlockRenderable } from "./blocks"
import { commandResultMarkdown } from "../../render/command-presentation"
import { projectCommandResult } from "../../render/command-results"
import { formatCost } from "../../render"

const MAX_ROW_TEXT = 4096
const USER_GUTTER = { topLeft: "▌", topRight: "▌", bottomLeft: "▌", bottomRight: "▌", horizontal: "▌", vertical: "▌", topT: "▌", bottomT: "▌", leftT: "▌", rightT: "▌", cross: "▌" } as const

export interface TranscriptRowOptions {
  readonly syntaxStyle: SyntaxStyle
  readonly treeSitterClient?: TreeSitterClient
  readonly onInteraction?: () => void
  readonly onOpenContent?: (source: TranscriptContentSource) => void
  readonly onOpenChild?: (child: Extract<TranscriptContent, { type: "subagent" }>) => void
  readonly reasoningExpanded?: boolean
  readonly onReasoningExpansion?: (id: string, expanded: boolean) => void
  readonly onExpansionChange: (id: string, expanded: boolean) => void
}

/** One stable semantic row; provider IR and lifetime event reduction stay outside rendering. */
export class TranscriptRowRenderable extends BoxRenderable {
  readonly header: TextRenderable
  readonly markdown: MarkdownRenderable
  readonly footer: TextRenderable
  readonly diffFooter: TextRenderable
  readonly presentationFooter: TextRenderable
  readonly shellCommand: CodeRenderable | null
  readonly shellOutput: TextRenderable | null
  readonly reasoning: ReasoningBlockRenderable
  readonly #options: TranscriptRowOptions
  readonly #theme: RottweilerTheme
  #retainedItem: TranscriptItem | null = null
  get #item(): TranscriptItem {
    const value = this.#retainedItem
    if (value === null) throw new Error("renderable model is released")
    return value
  }
  set #item(value: TranscriptItem) { this.#retainedItem = value }
  #expanded: boolean
  #selected = false
  #width = 0
  #source: TranscriptContentSource | null = null
  #diffSource: TranscriptContentSource | null = null
  #presentationSource: TranscriptContentSource | null = null

  override destroy(): void {
    this.#retainedItem = null
    this.#source = null; this.#diffSource = null; this.#presentationSource = null
    super.destroy()
  }

  constructor(ctx: RenderContext, theme: RottweilerTheme, item: TranscriptItem, options: TranscriptRowOptions, expanded?: boolean) {
    super(ctx, { id: `history-row:${item.id}`, width: "100%", flexDirection: "column", flexShrink: 0, marginTop: 1 })
    this.#item = item
    this.#options = options
    this.#theme = theme
    this.#expanded = expanded ?? item.content.type !== "tool"
    this.header = new TextRenderable(ctx, { content: "", fg: theme.textMuted, height: 1, selectable: true })
    this.markdown = new MarkdownRenderable(ctx, {
      content: "", width: "100%", fg: theme.markdownText, syntaxStyle: options.syntaxStyle,
      ...(options.treeSitterClient === undefined ? {} : { treeSitterClient: options.treeSitterClient }),
      conceal: true, concealCode: false, streaming: false, flexShrink: 0,
      internalBlockMode: "top-level", tableOptions: { style: "grid", widthMode: "full", wrapMode: "word" },
    })
    this.markdown.selectable = true
    this.reasoning = new ReasoningBlockRenderable(ctx, theme, options.syntaxStyle, {
      blockId: `history-reasoning:${item.id}`, content: "", width: 80,
      expanded: options.reasoningExpanded ?? true,
      onExpansionChange: expanded => options.onReasoningExpansion?.(item.id, expanded),
      onInteraction: () => options.onInteraction?.(),
    })
    this.shellCommand = item.content.type === "shell"
      ? new CodeRenderable(ctx, {
        content: "", fg: theme.text, width: "100%",
        syntaxStyle: options.syntaxStyle, filetype: "bash", conceal: false,
        ...(options.treeSitterClient === undefined ? {} : { treeSitterClient: options.treeSitterClient })
      }) : null
    this.shellOutput = item.content.type === "shell"
      ? new TextRenderable(ctx, { content: "", fg: theme.textMuted, selectable: true, width: "100%" }) : null
    this.diffFooter = new TextRenderable(ctx, { content: "Open child changes →", fg: theme.textMuted, height: 1, selectable: false, visible: false })
    bindSelectableClick(ctx, this.diffFooter, () => {
      if (this.#diffSource !== null) options.onOpenContent?.(this.#diffSource)
      options.onInteraction?.()
    })
    this.presentationFooter = new TextRenderable(ctx, { content: "", fg: theme.accent, height: 1, selectable: false, visible: false })
    bindSelectableClick(ctx, this.presentationFooter, () => {
      if (this.#presentationSource !== null) options.onOpenContent?.(this.#presentationSource)
      options.onInteraction?.()
    })
    this.footer = new TextRenderable(ctx, { content: "", fg: theme.textMuted, height: 1, selectable: false })
    bindSelectableClick(ctx, this.header, () => { this.toggle(); options.onInteraction?.() })
    bindSelectableClick(ctx, this.footer, () => {
      if (this.#item.content.type === "subagent") options.onOpenChild?.(this.#item.content)
      else if (this.#source !== null) options.onOpenContent?.(this.#source)
      options.onInteraction?.()
    })
    this.add(this.header)
    this.add(this.reasoning)
    this.add(this.markdown)
    if (this.shellCommand !== null) this.add(this.shellCommand)
    if (this.shellOutput !== null) this.add(this.shellOutput)
    this.add(this.diffFooter)
    this.add(this.presentationFooter)
    this.add(this.footer)
    this.#render()
  }

  get item(): TranscriptItem { return this.#item }
  get blockId(): string {
    return this.#item.content.type === "tool"
      ? `tool:${this.#item.content.invocation_id}` : `history:${this.#item.id}`
  }
  get expanded(): boolean { return this.#expanded }

  update(item: TranscriptItem, width: number): void {
    if (this.#item === item && this.#width === width) return
    this.#item = item
    this.#width = width
    this.#render()
  }

  toggle(): void {
    this.#expanded = !this.#expanded
    this.#options.onExpansionChange(this.#item.content.type === "tool" ? this.#item.content.invocation_id : this.blockId, this.#expanded)
    this.#render()
  }

  setSelected(selected: boolean): void {
    if (this.#selected === selected) return
    this.#selected = selected
    this.header.fg = selected ? this.#theme.accent : this.#theme.textMuted
    this.header.bg = selected ? this.#theme.backgroundElement : this.#theme.background
  }

  #render(): void {
    const content = this.#item.content
    const bodies: TranscriptBodyPreview[] = []
    let title: string
    let reasoning = ""
    this.#source = null
    this.#diffSource = null
    this.#presentationSource = null
    this.presentationFooter.visible = false
    switch (content.type) {
      case "turn_summary":
        title = `${content.status.replaceAll("_", " ")} · ${content.cost.kind === "subscription_quota" && content.cost.used == null ? "turn usage · " : ""}${formatCost(content.cost, content.usage)}`
        break
      case "conversation":
        title = content.role === "user" ? "You" : content.role === "assistant" ? "Rottweiler" : content.role
        this.#source = content.source
        for (const block of content.blocks) {
          if (block.type === "reasoning") reasoning += `${block.body.text}\n`
          else if (block.type !== "image") bodies.push(block.body)
        }
        if (content.blocks.some(block => block.type === "image")) title += " · image attached"
        break
      case "tool":
        title = `${this.#expanded ? "▾" : "▸"} ${content.name} · ${content.status.type === "running" ? "running" : content.status.is_error ? "failed" : "done"}`
        bodies.push(content.arguments)
        if (content.status.type === "finished") {
          bodies.push(content.status.output)
          this.#source = content.status.output.source
          if (content.status.presentation !== null) {
            this.#presentationSource = content.status.presentation.source
            this.presentationFooter.content = `${content.status.presentation.title} →`
            this.presentationFooter.visible = this.#expanded
          }
        } else this.#source = content.arguments.source
        if (content.diff !== null) bodies.push(content.diff)
        break
      case "command":
        title = `/${content.name}`
        bodies.push(content.message)
        this.#source = content.message.source
        break
      case "shell":
        title = `Terminal · ${content.active ? "running" : content.status === 0 ? "done" : `exit ${content.status ?? "—"}`}`
        if (content.command !== null) bodies.push(content.command)
        if (content.output !== null) bodies.push(content.output)
        this.#source = content.output?.source ?? content.command?.source ?? null
        break
      case "subagent":
        title = `Child agent · ${content.status.type === "running" ? "running" : content.status.status}`
        bodies.push(content.task)
        if (content.status.type === "finished") {
          bodies.push(content.status.result)
          if (content.status.touched_file_count > 0) title += ` · ${content.status.touched_file_count} files`
          this.#diffSource = content.status.diff
          if (this.#diffSource !== null) title += " · diff ready"
        }
        break
    }
    const text = content.type === "command"
      ? content.message.complete ? commandResultMarkdown(projectCommandResult(content.name, content.message.text))
        : content.message.format === "json" || /^[\s]*[\[{]/.test(content.message.text)
          ? "_Open complete content to inspect this structured result._" : content.message.text
      : bodies.map(body => body.format === "json" ? `\`\`\`json\n${body.text}\n\`\`\`` : body.text).join("\n\n")
    const clipped = text.length > MAX_ROW_TEXT
    this.header.content = content.type === "conversation" && content.role === "assistant"
      ? t`${fg(this.#theme.accent)("● ")}${bold(fg(this.#theme.text)("rottweiler"))}`
      : content.type === "conversation" && content.role === "user"
        ? t`${bold(fg(this.#theme.primary)("you"))}` : title
    this.markdown.content = this.#expanded && content.type !== "shell" ? clip(text) : ""
    this.markdown.visible = this.#expanded && text.length > 0 && content.type !== "shell"
    this.reasoning.visible = this.#expanded && reasoning.length > 0
    this.reasoning.update(clip(reasoning), false, Math.max(20, this.#width))
    if (content.type === "shell" && this.shellCommand !== null && this.shellOutput !== null) {
      this.shellCommand.content = content.command === null ? "" : `$ ${clip(content.command.text)}`
      this.shellOutput.content = content.output === null ? "" : clip(content.output.text)
      this.shellCommand.visible = this.#expanded && content.command !== null
      this.shellOutput.visible = this.#expanded && content.output !== null
    }
    const incomplete = clipped || bodies.some(body => !body.complete)
      || (content.type === "conversation" && content.omitted_blocks)
    this.diffFooter.visible = this.#diffSource !== null
    this.footer.visible = content.type === "subagent" || (this.#source !== null && (incomplete || content.type === "tool"))
    this.footer.content = content.type === "subagent" ? "Open child transcript →" : incomplete ? "Preview · open complete content →" : "Open content →"
    const user = content.type === "conversation" && content.role === "user"
    this.border = user ? ["left"] : false
    if (user) { this.customBorderChars = USER_GUTTER; this.borderColor = this.#theme.primary }
    this.backgroundColor = this.#theme.background
    this.paddingX = user ? 1 : 0
    this.markdown.paddingLeft = content.type === "conversation" && content.role === "assistant" ? 2 : 0
    this.marginTop = this.#item.ordinal === "0" ? 0 : 1
  }
}

function clip(text: string): string {
  if (text.length <= MAX_ROW_TEXT) return text
  const end = text.charCodeAt(MAX_ROW_TEXT - 1)
  return `${text.slice(0, end >= 0xd800 && end <= 0xdbff ? MAX_ROW_TEXT - 1 : MAX_ROW_TEXT)}\n…`
}
