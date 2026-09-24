import { TextRenderable } from "../text"
import { bindSelectableClick } from "../selectable-click"
import { BoxRenderable, CodeRenderable, MarkdownRenderable, type RenderContext, type SyntaxStyle, type TreeSitterClient } from "@opentui/core"
import type { TranscriptBodyPreview, TranscriptContent, TranscriptContentSource, TranscriptItem } from "../../protocol"
import type { RottweilerTheme } from "../../theme"
import { ReasoningBlockRenderable } from "./blocks"
import { ToolBlockRenderable } from "./tool-block"
import { commandResultMarkdown } from "../../render/command-presentation"
import { projectCommandResult } from "../../render/command-results"
import { turnEndLine } from "../../render/turn-end"
import { childResultTitle, parseChildResult, shortTask, type ChildLabel, type ChildResult } from "../../render/child-result"

const MAX_ROW_TEXT = 4096
const OPEN_FULL = "… click to open full content"

export interface TranscriptRowOptions {
  readonly syntaxStyle: SyntaxStyle
  readonly treeSitterClient?: TreeSitterClient
  readonly onInteraction?: () => void
  readonly onOpenContent?: (source: TranscriptContentSource) => void
  readonly onOpenChild?: (child: Extract<TranscriptContent, { type: "subagent" }>) => void
  readonly reasoningExpanded?: boolean
  readonly onReasoningExpansion?: (id: string, expanded: boolean) => void
  readonly onExpansionChange: (id: string, expanded: boolean) => void
  /** Agent name and task of a child, as far as the client knows them. */
  readonly childLabel?: (subagentId: string) => ChildLabel
}

/**
 * One transcript item. Messages, commands, shells, children, and turn endings
 * render here; tool items delegate to the same `ToolBlockRenderable` the live
 * tail uses, so a finished turn keeps its exact appearance when it moves from
 * the streaming tail into durable history.
 */
export class TranscriptRowRenderable extends BoxRenderable {
  readonly #header: TextRenderable
  readonly markdown: MarkdownRenderable
  readonly prefix: TextRenderable
  readonly #footer: TextRenderable
  readonly diffFooter: TextRenderable
  readonly shellCommand: CodeRenderable | null
  readonly shellOutput: TextRenderable | null
  readonly reasoning: ReasoningBlockRenderable
  readonly tool: ToolBlockRenderable | null
  readonly #message: BoxRenderable
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
  #follows: TranscriptContent["type"] | null = null
  #shadowed = false
  #empty = false
  #source: TranscriptContentSource | null = null
  #diffSource: TranscriptContentSource | null = null

  override destroy(): void {
    this.#retainedItem = null
    this.#source = null; this.#diffSource = null
    super.destroy()
  }

  constructor(ctx: RenderContext, theme: RottweilerTheme, item: TranscriptItem, options: TranscriptRowOptions, expanded?: boolean) {
    super(ctx, { id: `history-row:${item.id}`, width: "100%", flexDirection: "column", flexShrink: 0, marginTop: 1 })
    this.#item = item
    this.#options = options
    this.#theme = theme
    this.#expanded = expanded ?? childResultOf(item) === null
    this.#header = new TextRenderable(ctx, { content: "", fg: theme.textMuted, height: 1, selectable: true, visible: false })
    this.#message = new BoxRenderable(ctx, { width: "100%", flexDirection: "row", flexShrink: 0 })
    this.prefix = new TextRenderable(ctx, { content: "", fg: theme.primary, width: 2, flexShrink: 0, selectable: false })
    this.markdown = new MarkdownRenderable(ctx, {
      content: "", flexGrow: 1, flexShrink: 1, fg: theme.markdownText, syntaxStyle: options.syntaxStyle,
      ...(options.treeSitterClient === undefined ? {} : { treeSitterClient: options.treeSitterClient }),
      conceal: true, concealCode: false, streaming: false,
      internalBlockMode: "top-level", tableOptions: { style: "grid", widthMode: "full", wrapMode: "word" },
    })
    this.markdown.selectable = true
    this.#message.add(this.prefix)
    this.#message.add(this.markdown)
    this.reasoning = new ReasoningBlockRenderable(ctx, theme, options.syntaxStyle, {
      blockId: `history-reasoning:${item.id}`, content: "", width: 80,
      expanded: options.reasoningExpanded ?? true,
      onExpansionChange: expanded => options.onReasoningExpansion?.(item.id, expanded),
      onInteraction: () => options.onInteraction?.(),
    })
    this.tool = item.content.type === "tool"
      ? new ToolBlockRenderable(ctx, theme, item.content, expanded, value => options.onExpansionChange(this.#toolId(), value), {
        syntaxStyle: options.syntaxStyle,
        ...(options.treeSitterClient === undefined ? {} : { treeSitterClient: options.treeSitterClient }),
        ...(options.onOpenContent === undefined ? {} : { onOpenContent: options.onOpenContent }),
      }) : null
    this.shellCommand = item.content.type === "shell"
      ? new CodeRenderable(ctx, {
        content: "", fg: theme.text, width: "100%", marginLeft: 2,
        syntaxStyle: options.syntaxStyle, filetype: "bash", conceal: false,
        ...(options.treeSitterClient === undefined ? {} : { treeSitterClient: options.treeSitterClient })
      }) : null
    this.shellOutput = item.content.type === "shell"
      ? new TextRenderable(ctx, { content: "", fg: theme.textMuted, selectable: true, width: "100%", marginLeft: 2 }) : null
    this.diffFooter = new TextRenderable(ctx, { content: "Open child changes →", fg: theme.accent, height: 1, selectable: false, visible: false, marginLeft: 2 })
    bindSelectableClick(ctx, this.diffFooter, () => {
      if (this.#diffSource !== null) options.onOpenContent?.(this.#diffSource)
      options.onInteraction?.()
    })
    this.#footer = new TextRenderable(ctx, { content: "", fg: theme.textMuted, height: 1, selectable: false, visible: false, marginLeft: 2 })
    bindSelectableClick(ctx, this.#header, () => { this.toggle(); options.onInteraction?.() })
    // A child row is one line; opening it shows the child's own transcript.
    bindSelectableClick(ctx, this.#footer, () => {
      if (this.#item.content.type === "subagent") options.onOpenChild?.(this.#item.content)
      else if (this.#source !== null) options.onOpenContent?.(this.#source)
      options.onInteraction?.()
    })
    if (this.tool !== null) {
      this.add(this.tool)
    } else {
      this.add(this.#header)
      this.add(this.reasoning)
      this.add(this.#message)
      if (this.shellCommand !== null) this.add(this.shellCommand)
      if (this.shellOutput !== null) this.add(this.shellOutput)
      this.add(this.diffFooter)
      this.add(this.#footer)
    }
    this.#render()
  }

  get item(): TranscriptItem { return this.#item }
  /** Tool rows present the tool block's header so selection highlights one line. */
  get header(): TextRenderable { return this.tool?.header ?? this.#header }
  get blockId(): string { return this.tool?.blockId ?? `history:${this.#item.id}` }
  get expanded(): boolean { return this.tool?.expanded ?? this.#expanded }
  /** Child-result cards are keyboard-navigable blocks like tool rows. */
  get isChildResult(): boolean { return childResultOf(this.#item) !== null }
  /** The row's one open-content link: full output, a result view, or a child transcript. */
  get footer(): TextRenderable { return this.tool?.truncationMarker ?? this.#footer }

  /** `follows` is the content type of the preceding row, which sets vertical rhythm. */
  update(item: TranscriptItem, width: number, follows: TranscriptContent["type"] | null = this.#follows): void {
    if (this.#item === item && this.#width === width && this.#follows === follows) return
    this.#item = item
    this.#width = width
    this.#follows = follows
    this.#render()
  }

  /** A running tool that the live tail is drawing is hidden here to avoid a duplicate row. */
  setShadowed(shadowed: boolean): void {
    if (this.#shadowed === shadowed) return
    this.#shadowed = shadowed
    this.visible = !shadowed && !this.#empty
  }

  toggle(): void {
    if (this.tool !== null) { this.tool.toggle(); return }
    if (this.#item.content.type === "subagent") { this.#options.onOpenChild?.(this.#item.content); return }
    this.#expanded = !this.#expanded
    this.#options.onExpansionChange(this.blockId, this.#expanded)
    this.#render()
  }

  setSelected(selected: boolean): void {
    if (this.tool !== null) { this.tool.setSelected(selected); return }
    if (this.#selected === selected) return
    this.#selected = selected
    this.#header.bg = selected ? this.#theme.backgroundElement : this.#theme.background
  }

  #toolId(): string {
    const content = this.#item.content
    return content.type === "tool" ? content.invocation_id : this.blockId
  }

  #render(): void {
    const content = this.#item.content
    this.marginTop = this.#item.ordinal === "0" || (content.type === "tool" && this.#follows === "tool") ? 0 : 1
    if (content.type === "tool") {
      this.tool?.update(content, Math.max(20, this.#width))
      this.#setEmpty(false)
      return
    }
    const bodies: TranscriptBodyPreview[] = []
    let header = ""
    let reasoning = ""
    let prefix = ""
    const child = childResultOf(this.#item)
    this.#source = null
    this.#diffSource = null
    this.#message.backgroundColor = this.#theme.background
    switch (content.type) {
      case "turn_summary": {
        const line = turnEndLine(content.status, content.cost, content.usage)
        this.#setEmpty(line === null)
        header = line?.text ?? ""
        this.#header.fg = line?.tone === "error" ? this.#theme.error : line?.tone === "warning" ? this.#theme.warning : this.#theme.textMuted
        break
      }
      case "conversation":
        this.#source = content.source
        if (child !== null) {
          header = childResultTitle(child, this.#options.childLabel?.(child.id) ?? { name: null, task: null })
          this.#header.fg = child.status === "completed" ? this.#theme.success
            : child.status === "failed" ? this.#theme.error : this.#theme.warning
          const report = content.blocks.find(block => block.type === "text")
          if (child.report !== "" && report?.type === "text") bodies.push({ ...report.body, text: child.report })
          break
        }
        for (const block of content.blocks) {
          if (block.type === "reasoning") reasoning += `${block.body.text}\n`
          else if (block.type !== "image") bodies.push(block.body)
        }
        if (content.role === "user") {
          prefix = "›"
          this.#message.backgroundColor = this.#theme.backgroundPanel
        }
        break
      case "command":
        header = `/${content.name}`
        bodies.push(content.message)
        this.#source = content.message.source
        break
      case "shell":
        header = `Terminal · ${content.active ? "running" : content.status === 0 ? "done" : `exit ${content.status ?? "—"}`}`
        if (content.command !== null) bodies.push(content.command)
        if (content.output !== null) bodies.push(content.output)
        this.#source = content.output?.source ?? content.command?.source ?? null
        break
      case "subagent": {
        // The report is shown once, in the child-result card that follows.
        const name = this.#options.childLabel?.(content.subagent_id).name ?? "Agent"
        const status = content.status.type === "running" ? "running" : content.status.status.replaceAll("_", " ")
        header = `↳ ${name} · ${shortTask(content.task.text)} · ${status}`
        if (content.status.type === "finished") {
          if (content.status.touched_file_count > 0) header += ` · ${content.status.touched_file_count} files`
          this.#diffSource = content.status.diff
          if (this.#diffSource !== null) header += " · diff ready"
        }
        break
      }
    }
    if (content.type !== "turn_summary") this.#setEmpty(false)
    const images = content.type === "conversation" && content.blocks.some(block => block.type === "image") ? "_image attached_\n\n" : ""
    const text = images + (content.type === "command"
      ? content.message.complete ? commandResultMarkdown(projectCommandResult(content.name, content.message.text))
        : content.message.format === "json" || /^[\s]*[\[{]/.test(content.message.text)
          ? "" : content.message.text
      : bodies.map(body => body.format === "json" ? `\`\`\`json\n${body.text}\n\`\`\`` : body.text).join("\n\n"))
    const clipped = text.length > MAX_ROW_TEXT
    const expanded = content.type !== "subagent" && ((content.type === "conversation" && child === null) || this.#expanded)
    this.#header.visible = header !== ""
    this.#header.content = header
    this.prefix.content = prefix
    this.markdown.content = expanded && content.type !== "shell" ? clip(text) : ""
    this.#message.visible = expanded && text.length > 0 && content.type !== "shell"
    this.reasoning.visible = reasoning.length > 0
    this.reasoning.marginTop = header === "" ? 0 : 1
    this.#message.marginTop = reasoning.length > 0 ? 1 : 0
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
    this.#footer.visible = this.#source !== null && incomplete && expanded
    this.#footer.content = OPEN_FULL
    this.#footer.fg = this.#theme.textMuted
  }

  #setEmpty(empty: boolean): void {
    this.#empty = empty
    this.visible = !this.#shadowed && !empty
  }
}

function childResultOf(item: TranscriptItem): ChildResult | null {
  const content = item.content
  if (content.type !== "conversation" || content.role !== "user") return null
  const first = content.blocks.find(block => block.type === "text")
  return first === undefined || first.type !== "text" ? null : parseChildResult(first.body.text)
}

function clip(text: string): string {
  if (text.length <= MAX_ROW_TEXT) return text
  const end = text.charCodeAt(MAX_ROW_TEXT - 1)
  return `${text.slice(0, end >= 0xd800 && end <= 0xdbff ? MAX_ROW_TEXT - 1 : MAX_ROW_TEXT)}\n…`
}
