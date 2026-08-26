import {
  BoxRenderable,
  ScrollBoxRenderable,
  TextRenderable,
  fg,
  t,
  type RenderContext,
} from "@opentui/core"

import {
  formatCost,
  getScrollAcceleration,
  stringCellWidth,
  truncateToCells,
  type ActivityPresentation,
  type ToolsWorkspacePresentation,
} from "../render"
import type { RottweilerTheme } from "../theme"

export interface ToolsWorkspaceOptions {
  readonly onOpenToolOutput: (toolCallId: string) => void
}

export class ToolActivityRowRenderable extends BoxRenderable {
  readonly key: string
  readonly header: TextRenderable
  readonly output: TextRenderable
  readonly marker: TextRenderable
  #model: ActivityPresentation
  #expanded: boolean
  #selected = false
  #theme: RottweilerTheme
  #availableWidth = 20
  #onOpenToolOutput: (toolCallId: string) => void

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    model: ActivityPresentation,
    onOpenToolOutput: (toolCallId: string) => void,
  ) {
    super(ctx, {
      id: model.key,
      width: "100%",
      flexDirection: "column",
      flexShrink: 0,
      border: ["left"],
      borderStyle: "single",
      borderColor: theme.borderSubtle,
      paddingLeft: 1,
      backgroundColor: theme.background,
    })
    this.key = model.key
    this.#model = model
    this.#theme = theme
    this.#onOpenToolOutput = onOpenToolOutput
    this.#expanded = model.kind === "foreground_shell" ? model.active : model.defaultExpanded
    this.header = new TextRenderable(ctx, {
      id: `${model.key}:header`,
      content: "",
      width: "100%",
      height: 1,
      flexShrink: 0,
      wrapMode: "none",
      truncate: true,
      selectable: true,
      fg: theme.text,
      bg: theme.background,
    })
    this.output = new TextRenderable(ctx, {
      id: `${model.key}:output`,
      content: "",
      width: "100%",
      height: 0,
      flexShrink: 0,
      wrapMode: "none",
      visible: false,
      selectable: true,
      fg: theme.textMuted,
      bg: theme.background,
    })
    this.marker = new TextRenderable(ctx, {
      id: `${model.key}:view-all`,
      content: "",
      width: "100%",
      height: 0,
      flexShrink: 0,
      wrapMode: "none",
      truncate: true,
      visible: false,
      selectable: true,
      fg: theme.textMuted,
      bg: theme.background,
    })
    this.header.onMouseUp = () => {
      const selection = this.ctx.getSelection()
      if (selection === null || selection.getSelectedText().trim() === "") {
        this.expand(!this.#expanded)
      }
    }
    this.marker.onMouseUp = () => {
      const selection = this.ctx.getSelection()
      if (selection !== null && selection.getSelectedText().trim() !== "") return
      this.openOutput()
    }
    this.add(this.header)
    this.add(this.output)
    this.add(this.marker)
    this.update(model, this.#availableWidth)
  }

  get expanded(): boolean {
    return this.#expanded
  }

  get model(): ActivityPresentation {
    return this.#model
  }

  update(model: ActivityPresentation, availableWidth: number): void {
    this.#model = model
    this.#availableWidth = Math.max(12, availableWidth)
    this.#renderHeader()
    this.#renderOutput()
  }

  expand(expanded: boolean): void {
    if (this.#expanded === expanded) return
    this.#expanded = expanded
    this.#renderHeader()
    this.#renderOutput()
  }

  setSelected(selected: boolean): void {
    if (this.#selected === selected) return
    this.#selected = selected
    const background = selected ? this.#theme.backgroundElement : this.#theme.background
    this.backgroundColor = background
    this.header.bg = background
    this.output.bg = background
    this.marker.bg = background
  }

  openOutput(): boolean {
    if (this.#model.kind !== "tool" || !this.#model.canOpenRetainedOutput) return false
    this.#onOpenToolOutput(this.#model.toolCallId)
    return true
  }

  #renderHeader(): void {
    const fold = this.#expanded ? "⌄" : "▸"
    if (this.#model.kind === "foreground_shell") {
      const name = "shell"
      const status = this.#model.active
        ? "live"
        : this.#model.status === null
          ? "complete"
          : `exit ${this.#model.status}`
      const content = alignedHeader(
        fold,
        name,
        this.#model.command,
        status,
        this.#availableWidth,
        true,
      )
      this.header.content = t`${fg(this.#theme.textMuted)(`${fold} `)}${fg(this.#theme.secondary)(`${content.name}${content.subject === "" ? "" : "  "}`)}${content.subject === "" ? "" : fg(this.#theme.text)(content.subject)}${content.spacing}${fg(this.#model.active ? this.#theme.info : this.#theme.textMuted)(content.outcome)}`
      return
    }
    const outcome = compactOutcome(this.#model, this.#availableWidth)
    const content = alignedHeader(
      fold,
      this.#model.name.replaceAll("_", "-"),
      this.#model.subject,
      outcome.label,
      this.#availableWidth,
      outcome.show,
    )
    const outcomeColor = this.#model.outcome.kind === "denied" || this.#model.outcome.kind === "failed"
      ? this.#theme.error
      : this.#model.outcome.kind === "awaiting_approval"
        ? this.#theme.warning
        : this.#model.outcome.kind === "running"
          ? this.#theme.info
          : this.#theme.success
    this.header.content = t`${fg(this.#theme.textMuted)(`${fold} `)}${fg(this.#theme.secondary)(`${content.name}${content.subject === "" ? "" : "  "}`)}${content.subject === "" ? "" : fg(this.#theme.text)(content.subject)}${content.spacing}${content.outcome === "" ? "" : fg(outcomeColor)(content.outcome)}`
  }

  #renderOutput(): void {
    const output = this.#model.output
    const visible = this.#expanded && output.kind === "text"
    this.output.visible = visible
    this.output.height = visible ? output.visibleLineCount : 0
    this.output.content = visible ? output.text : ""
    const showMarker = visible && (
      output.hiddenRetainedLineCount > 0 ||
      output.sourceTruncated
    )
    this.marker.visible = showMarker
    this.marker.height = showMarker ? 1 : 0
    if (!showMarker) {
      this.marker.content = ""
      return
    }
    const retained = output.hiddenRetainedLineCount > 0
      ? `${output.hiddenRetainedLineCount} more retained line${output.hiddenRetainedLineCount === 1 ? "" : "s"}`
      : "retained source is truncated"
    const action = this.#model.kind === "tool" && this.#model.canOpenRetainedOutput
      ? " · view all"
      : ""
    this.marker.content = truncateToCells(`… ${retained}${action}`, this.#availableWidth)
  }
}

export class ToolsWorkspaceRenderable extends BoxRenderable {
  readonly activityPane: BoxRenderable
  readonly header: TextRenderable
  readonly activityScroller: ScrollBoxRenderable
  readonly activeStatus: TextRenderable
  readonly queueBlock: TextRenderable
  readonly turnRail: BoxRenderable
  readonly turnSummary: TextRenderable
  readonly queueSummary: TextRenderable
  #rows = new Map<string, ToolActivityRowRenderable>()
  #model: ToolsWorkspacePresentation | null = null
  #selectedRowKey: string | null = null
  #options: ToolsWorkspaceOptions
  #theme: RottweilerTheme
  #terminalWidth: number
  #terminalHeight: number

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    options: ToolsWorkspaceOptions,
  ) {
    super(ctx, {
      id: "tools-workspace",
      width: "100%",
      height: "100%",
      flexDirection: "row",
      backgroundColor: theme.background,
    })
    this.#options = options
    this.#theme = theme
    this.#terminalWidth = ctx.width
    this.#terminalHeight = ctx.height
    this.activityPane = new BoxRenderable(ctx, {
      id: "tools-activity-pane",
      width: "100%",
      height: "100%",
      flexDirection: "column",
      backgroundColor: theme.background,
      paddingX: 1,
    })
    this.header = new TextRenderable(ctx, {
      id: "tools-header",
      content: t`${fg(theme.primary)("● rottweiler")}${fg(theme.textMuted)("  running tools")}`,
      width: "100%",
      height: 1,
      flexShrink: 0,
      wrapMode: "none",
      selectable: true,
      fg: theme.text,
    })
    const spacer = new TextRenderable(ctx, {
      id: "tools-header-spacer",
      content: "",
      width: "100%",
      height: 1,
      flexShrink: 0,
    })
    this.activityScroller = new ScrollBoxRenderable(ctx, {
      id: "tools-activity-scroll",
      width: "100%",
      height: 1,
      flexShrink: 0,
      scrollY: true,
      scrollX: false,
      stickyScroll: true,
      stickyStart: "bottom",
      scrollAcceleration: getScrollAcceleration(),
      viewportCulling: true,
      contentOptions: { flexDirection: "column", width: "100%" },
      verticalScrollbarOptions: {
        showArrows: false,
        trackOptions: { backgroundColor: theme.background },
      },
    })
    this.activeStatus = new TextRenderable(ctx, {
      id: "tools-active-status",
      content: "",
      width: "100%",
      height: 1,
      flexShrink: 0,
      wrapMode: "none",
      truncate: true,
      selectable: true,
      fg: theme.textMuted,
    })
    this.queueBlock = new TextRenderable(ctx, {
      id: "tools-queue-block",
      content: "",
      width: "100%",
      height: 0,
      flexShrink: 0,
      wrapMode: "word",
      visible: false,
      selectable: true,
      fg: theme.warning,
    })
    this.turnRail = new BoxRenderable(ctx, {
      id: "tools-turn-rail",
      width: 36,
      height: "100%",
      flexShrink: 0,
      flexDirection: "column",
      border: ["left"],
      borderStyle: "single",
      borderColor: theme.borderSubtle,
      backgroundColor: theme.backgroundPanel,
    })
    this.turnSummary = new TextRenderable(ctx, {
      id: "tools-turn-summary",
      content: "",
      width: "100%",
      flexShrink: 0,
      wrapMode: "word",
      selectable: true,
      fg: theme.text,
    })
    this.queueSummary = new TextRenderable(ctx, {
      id: "tools-queue-summary",
      content: "",
      width: "100%",
      flexShrink: 0,
      wrapMode: "word",
      selectable: true,
      fg: theme.textMuted,
    })
    this.activityPane.add(this.header)
    this.activityPane.add(spacer)
    this.activityPane.add(this.activityScroller)
    this.activityPane.add(this.activeStatus)
    this.activityPane.add(this.queueBlock)
    this.turnRail.add(this.turnSummary)
    this.turnRail.add(this.queueSummary)
    this.add(this.activityPane)
    this.add(this.turnRail)
    this.#layout()
  }

  get mountedRowCount(): number {
    return this.#rows.size
  }

  get mountedRowKeys(): readonly string[] {
    return this.activityScroller.getChildren().map((row) => row.id)
  }

  get selectedRowKey(): string | null {
    return this.#selectedRowKey
  }

  rowForKey(key: string): ToolActivityRowRenderable | undefined {
    return this.#rows.get(key)
  }

  update(model: ToolsWorkspacePresentation): void {
    this.#model = model
    const desiredKeys = new Set<string>(model.rows.map((row) => row.key))
    for (const [key, row] of this.#rows) {
      if (desiredKeys.has(key)) continue
      this.activityScroller.remove(row)
      this.#rows.delete(key)
      row.destroyRecursively()
    }
    const availableWidth = this.#activityContentWidth()
    for (const rowModel of model.rows) {
      let row = this.#rows.get(rowModel.key)
      if (row === undefined) {
        row = new ToolActivityRowRenderable(
          this.ctx,
          this.#theme,
          rowModel,
          this.#options.onOpenToolOutput,
        )
        this.#rows.set(rowModel.key, row)
        this.activityScroller.add(row)
      } else {
        row.update(rowModel, availableWidth)
      }
    }
    const currentOrder = this.activityScroller.getChildren().map((row) => row.id)
    const desiredOrder = model.rows.map((row) => row.key)
    if (!sameOrder(currentOrder, desiredOrder)) {
      const orderedRows = desiredOrder.flatMap((key) => {
        const row = this.#rows.get(key)
        return row === undefined ? [] : [row]
      })
      for (const row of orderedRows) this.activityScroller.remove(row)
      for (const row of orderedRows) this.activityScroller.add(row)
    }
    if (this.#selectedRowKey !== null && !desiredKeys.has(this.#selectedRowKey)) {
      this.#selectedRowKey = null
    }
    this.#syncSelection()
    this.#updateText()
    this.#layout()
  }

  resizeForTerminal(width: number, height: number): void {
    this.#terminalWidth = Math.max(1, width)
    this.#terminalHeight = Math.max(1, height)
    this.width = this.#terminalWidth
    this.height = this.#terminalHeight
    this.#layout()
  }

  selectPreviousBlock(): void {
    const keys = this.#visibleRowKeys()
    if (keys.length === 0) return
    const index = this.#selectedRowKey === null ? -1 : keys.indexOf(this.#selectedRowKey)
    this.#selectedRowKey = index < 0 ? keys.at(-1) ?? null : keys[Math.max(0, index - 1)] ?? null
    this.#syncSelection(true)
  }

  selectNextBlock(): void {
    const keys = this.#visibleRowKeys()
    if (keys.length === 0) return
    const index = this.#selectedRowKey === null ? -1 : keys.indexOf(this.#selectedRowKey)
    this.#selectedRowKey = keys[Math.min(keys.length - 1, index + 1)] ?? null
    this.#syncSelection(true)
  }

  toggleSelectedBlock(): void {
    const row = this.#selectedRowKey === null ? undefined : this.#rows.get(this.#selectedRowKey)
    if (row === undefined) return
    row.expand(!row.expanded)
    this.#syncSelection(true)
  }

  openSelectedOutput(): boolean {
    const row = this.#selectedRowKey === null ? undefined : this.#rows.get(this.#selectedRowKey)
    return row?.openOutput() ?? false
  }

  protected override onResize(width: number, height: number): void {
    this.#terminalWidth = Math.max(1, width)
    this.#terminalHeight = Math.max(1, height)
    this.#layout()
  }

  #layout(): void {
    const railVisible = this.#terminalWidth >= 100 && this.#terminalHeight >= 12
    const activityWidth = railVisible ? Math.max(1, this.#terminalWidth - 36) : this.#terminalWidth
    this.turnRail.visible = railVisible
    this.turnRail.width = railVisible ? 36 : 0
    this.activityPane.width = activityWidth
    this.activityPane.height = this.#terminalHeight
    this.turnRail.height = this.#terminalHeight

    const queued = this.#model?.queuedMessages.length ?? 0
    const spacer = this.activityPane.findDescendantById("tools-header-spacer")
    const compact = this.#terminalHeight < 12
    const short = this.#terminalHeight >= 12 && this.#terminalHeight < 18
    if (spacer !== undefined) {
      spacer.height = compact || short ? 0 : 1
      spacer.visible = !compact && !short
    }
    this.activeStatus.height = compact ? 0 : 1
    this.activeStatus.visible = !compact
    const queueHeight = queued === 0 ? 0 : compact ? 1 : short ? 2 : 4
    this.queueBlock.height = queueHeight
    this.queueBlock.visible = queueHeight > 0
    const fixedRows = 1 + (compact || short ? 0 : 1) + (compact ? 0 : 1) + queueHeight
    this.activityScroller.height = Math.max(1, this.#terminalHeight - fixedRows)

    const availableWidth = Math.max(12, activityWidth - 4)
    for (const [key, row] of this.#rows) {
      const model = this.#model?.rows.find((candidate) => candidate.key === key)
      if (model !== undefined) row.update(model, availableWidth)
    }
  }

  #activityContentWidth(): number {
    const railVisible = this.#terminalWidth >= 100 && this.#terminalHeight >= 12
    const activityWidth = railVisible
      ? Math.max(1, this.#terminalWidth - 36)
      : this.#terminalWidth
    return Math.max(12, activityWidth - 4)
  }

  #updateText(): void {
    const model = this.#model
    if (model === null) return
    if (model.replay) {
      this.activeStatus.content = "replay · read only"
    } else if (model.turn.kind === "running") {
      this.activeStatus.content = "● running · Esc Esc to interrupt"
    } else {
      this.activeStatus.content = model.turn.kind === "finished" ? "turn complete" : ""
    }
    this.queueBlock.content = queueBlockText(model, this.#terminalHeight)
    this.turnSummary.content = turnSummaryText(model)
    this.turnSummary.height = lineCount(this.turnSummary.plainText)
    this.queueSummary.content = queueSummaryText(model)
    this.queueSummary.height = lineCount(this.queueSummary.plainText)
  }

  #visibleRowKeys(): readonly string[] {
    return this.#model?.rows.map((row) => row.key) ?? []
  }

  #syncSelection(scrollIntoView = false): void {
    for (const [key, row] of this.#rows) row.setSelected(key === this.#selectedRowKey)
    if (scrollIntoView && this.#selectedRowKey !== null) {
      this.activityScroller.scrollChildIntoView(this.#selectedRowKey)
    }
  }

}

function compactOutcome(
  model: Extract<ActivityPresentation, { readonly kind: "tool" }>,
  width: number,
): { readonly label: string; readonly show: boolean } {
  if (width < 28) return { label: "", show: false }
  if (width < 48) return { label: outcomeGlyph(model.outcome.kind), show: true }
  const elapsed = model.elapsed.kind === "known" ? ` · ${model.elapsed.label}` : ""
  return { label: `${model.outcome.label}${elapsed}`, show: true }
}

function outcomeGlyph(kind: Extract<ActivityPresentation, { readonly kind: "tool" }>['outcome']['kind']): string {
  if (kind === "running") return "●"
  if (kind === "awaiting_approval") return "?"
  if (kind === "succeeded") return "✓"
  return "✕"
}

function alignedHeader(
  fold: string,
  rawName: string,
  rawSubject: string,
  rawOutcome: string,
  availableWidth: number,
  showOutcome: boolean,
): { readonly name: string; readonly subject: string; readonly spacing: string; readonly outcome: string } {
  const name = truncateToCells(rawName, 14)
  const outcome = showOutcome ? truncateToCells(rawOutcome, Math.max(1, Math.floor(availableWidth / 2))) : ""
  const fixedWidth = stringCellWidth(fold) + 1 + stringCellWidth(name) + (outcome === "" ? 0 : stringCellWidth(outcome) + 2)
  const subjectBudget = Math.max(0, availableWidth - fixedWidth - 2)
  const subject = truncateToCells(rawSubject.replace(/\s+/g, " ").trim(), subjectBudget)
  const usedWidth = stringCellWidth(fold) + 1 + stringCellWidth(name) + (subject === "" ? 0 : 2 + stringCellWidth(subject)) + stringCellWidth(outcome)
  const spacing = outcome === "" ? "" : " ".repeat(Math.max(2, availableWidth - usedWidth))
  return { name, subject, spacing, outcome }
}

function turnSummaryText(model: ToolsWorkspacePresentation): string {
  if (model.turn.kind === "none") return ""
  const rows = [
    "THIS TURN",
    "",
    `tools    ${model.turn.toolCount}`,
    `live     ${model.turn.liveCount}`,
    `denied   ${model.turn.deniedCount}`,
    `elapsed  ${model.turn.elapsed.label}`,
  ]
  if (model.turn.kind === "finished") {
    rows.push(
      `input    ${model.turn.usage.input_tokens}`,
      `output   ${model.turn.usage.output_tokens}`,
      `cost     ${formatCost(model.turn.cost, model.turn.usage)}`,
    )
  }
  return rows.join("\n")
}

function queueBlockText(model: ToolsWorkspacePresentation, height: number): string {
  const [head, ...later] = model.queuedMessages
  if (head === undefined) return ""
  const count = model.queuedMessages.length
  if (height < 12) return `${count} message${count === 1 ? "" : "s"} queued · next: ${head.content}`
  if (height < 18) return `${count} message${count === 1 ? "" : "s"} queued\nnext: ${head.content}`
  return [
    `${count} message${count === 1 ? "" : "s"} queued`,
    `Next sends when this turn ends: “${head.content}”`,
    later.length === 0
      ? "Only the next message is consumed."
      : `${later.length} later message${later.length === 1 ? " remains" : "s remain"} queued.`,
  ].join("\n")
}

function queueSummaryText(model: ToolsWorkspacePresentation): string {
  const [head, ...later] = model.queuedMessages
  if (head === undefined) return ""
  return [
    "",
    `QUEUED ${model.queuedMessages.length}`,
    `next  ${head.content}`,
    later.length === 0 ? "sends when this turn ends" : `${later.length} later after the next turn`,
  ].join("\n")
}

function lineCount(text: string): number {
  return text === "" ? 0 : text.split("\n").length
}

function sameOrder(left: readonly string[], right: readonly string[]): boolean {
  return left.length === right.length && left.every((value, index) => value === right[index])
}
