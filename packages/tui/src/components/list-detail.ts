import { TextRenderable } from "./text"
import {
  BoxRenderable,
  InputRenderable,
  InputRenderableEvents,
  StyledText,
  bold,
  fg,
  type KeyEvent,
  type RenderContext,
} from "@opentui/core"

import { truncateToCells } from "../render"
import type { RottweilerTheme } from "../theme"

export interface ListDetailSectionRow {
  readonly kind: "section"
  readonly id: string
  readonly label: string
}

export interface ListDetailItemRow<Action> {
  readonly kind: "item"
  readonly id: string
  readonly label: string
  readonly matchSpans: readonly (readonly [start: number, end: number])[]
  readonly detail: {
    readonly title: string
    readonly description: string
    readonly meta: string
  }
  readonly action: Action
}

export type ListDetailRow<Action> = ListDetailSectionRow | ListDetailItemRow<Action>

export interface ListDetailPresentation<Action> {
  readonly title: string
  readonly query: string
  readonly rows: readonly ListDetailRow<Action>[]
  readonly selectedId: string | null
  readonly status: string
  readonly emptyCopy?: string
  readonly notice?: {
    readonly message: string
    readonly tone: "muted" | "warning" | "error"
  } | null
}

export interface ListDetailHandlers<Action> {
  readonly onSelect: (action: Action) => void
  readonly onQuery?: (query: string) => void
  readonly onSelection?: (selectedId: string | null) => void
  readonly onRetry?: () => void
}

export interface ListDetailOptions<Action> {
  readonly surfaceLayout?: "modal" | "primary"
  readonly splitListWidth?: number
  readonly splitMinWidth?: number
  readonly compactMinHeight?: number
  readonly inputPlaceholder?: string
  readonly emptyCopy?: string
  readonly showCompactDetail?: boolean
  readonly surfaceBackground?: string
  readonly renderRow?: (
    row: ListDetailItemRow<Action>,
    selected: boolean,
    availableWidth: number,
  ) => StyledText | string
  readonly renderDetail?: (
    row: ListDetailItemRow<Action>,
    availableWidth: number,
    availableHeight: number,
  ) => StyledText | string
}

export class ListDetailRenderable<Action> extends BoxRenderable {
  readonly heading: TextRenderable
  readonly input: InputRenderable
  readonly rule: TextRenderable
  readonly listPane: BoxRenderable
  readonly divider: TextRenderable
  readonly detailPane: BoxRenderable
  readonly detail: TextRenderable
  readonly compactDetail: TextRenderable
  readonly footer: TextRenderable
  readonly rowViews: TextRenderable[] = []

  #rows: readonly ListDetailRow<Action>[] = []
  #selectedId: string | null = null
  #scrollOffset = 0
  #pressedRowId: string | null = null
  #handlers: ListDetailHandlers<Action> | null = null
  #theme: RottweilerTheme
  #options: ListDetailOptions<Action>
  #surfaceBackground: string
  #layoutMode: "split" | "single" = "split"
  #visibleRows = 20
  #listWidth = 52
  #emptyCopy: string | undefined
  #onKey = (key: KeyEvent) => {
    if (!this.visible) return
    const plain = !key.ctrl && !key.meta && !key.option && !key.shift
    const controlOnly = key.ctrl && !key.meta && !key.option && !key.shift
    let handled = true
    if (controlOnly && key.name === "r" && this.#handlers?.onRetry !== undefined) {
      this.#handlers.onRetry()
    } else if ((plain && key.name === "up") || (controlOnly && key.name === "p")) {
      this.moveSelection(-1)
    } else if ((plain && key.name === "down") || (controlOnly && key.name === "n")) {
      this.moveSelection(1)
    } else if (plain && key.name === "pageup") {
      this.moveSelection(-this.#visibleRows, false)
    } else if (plain && key.name === "pagedown") {
      this.moveSelection(this.#visibleRows, false)
    } else if (plain && key.name === "home") {
      this.moveToBoundary(false)
    } else if (plain && key.name === "end") {
      this.moveToBoundary(true)
    } else if (plain && (key.name === "return" || key.name === "kpenter")) {
      this.activateSelected()
    } else {
      // Escape stays owned by the application so Vim and focus restoration
      // follow the same lifecycle as every other modal.
      handled = false
    }
    if (!handled) return
    key.preventDefault()
    key.stopPropagation()
  }

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    options: ListDetailOptions<Action> = {},
  ) {
    const surfaceBackground = options.surfaceBackground ?? theme.backgroundElement
    super(ctx, {
      id: "list-detail",
      position: "absolute",
      backgroundColor: surfaceBackground,
      visible: false,
      zIndex: 20,
      overflow: "hidden",
    })
    this.#theme = theme
    this.#options = options
    this.#surfaceBackground = surfaceBackground
    this.heading = new TextRenderable(ctx, {
      id: "list-detail-heading",
      position: "absolute",
      height: 1,
      fg: theme.text,
      truncate: true,
    })
    this.input = new InputRenderable(ctx, {
      id: "list-detail-query",
      position: "absolute",
      placeholder: options.inputPlaceholder ?? "Type to filter commands…",
      backgroundColor: surfaceBackground,
      focusedBackgroundColor: surfaceBackground,
      textColor: theme.text,
      focusedTextColor: theme.text,
    })
    this.rule = new TextRenderable(ctx, {
      id: "list-detail-rule",
      position: "absolute",
      height: 1,
      fg: theme.borderSubtle,
      truncate: true,
    })
    this.listPane = new BoxRenderable(ctx, {
      id: "list-detail-list",
      position: "absolute",
      overflow: "hidden",
      backgroundColor: surfaceBackground,
    })
    this.divider = new TextRenderable(ctx, {
      id: "list-detail-divider",
      position: "absolute",
      width: 1,
      fg: theme.borderSubtle,
      truncate: true,
    })
    this.detailPane = new BoxRenderable(ctx, {
      id: "list-detail-detail-pane",
      position: "absolute",
      overflow: "hidden",
      backgroundColor: surfaceBackground,
      paddingLeft: 1,
    })
    this.detail = new TextRenderable(ctx, {
      id: "list-detail-detail",
      width: "100%",
      height: "100%",
      fg: theme.text,
      wrapMode: "word",
    })
    this.detailPane.add(this.detail)
    this.compactDetail = new TextRenderable(ctx, {
      id: "list-detail-compact-detail",
      position: "absolute",
      height: 1,
      fg: theme.textMuted,
      truncate: true,
      visible: false,
    })
    this.footer = new TextRenderable(ctx, {
      id: "list-detail-footer",
      position: "absolute",
      height: 1,
      fg: theme.textMuted,
      truncate: true,
    })
    this.add(this.heading)
    this.add(this.input)
    this.add(this.rule)
    this.add(this.listPane)
    this.add(this.divider)
    this.add(this.detailPane)
    this.add(this.compactDetail)
    this.add(this.footer)
    this.input.on(InputRenderableEvents.INPUT, (query: string) => this.#handlers?.onQuery?.(query))
    this.input.on(InputRenderableEvents.ENTER, () => this.activateSelected())
    this.listPane.onMouseScroll = (event) => {
      const direction = event.scroll?.direction
      if (direction !== "up" && direction !== "down") return
      this.scrollViewport(direction === "up" ? -1 : 1)
      event.preventDefault()
      event.stopPropagation()
    }
    ctx.keyInput.on("keypress", this.#onKey)
    this.resizeForTerminal(ctx.width, ctx.height)
  }

  get selectedId(): string | null {
    return this.#selectedId
  }

  get scrollOffset(): number {
    return this.#scrollOffset
  }

  get layoutMode(): "split" | "single" {
    return this.#layoutMode
  }

  get itemIds(): readonly string[] {
    return this.#rows.flatMap((row) => row.kind === "item" ? [row.id] : [])
  }

  get sectionLabels(): readonly string[] {
    return this.#rows.flatMap((row) => row.kind === "section" ? [row.label] : [])
  }

  get visibleRowCount(): number {
    return this.#visibleRows
  }

  get selectedRowIndex(): number {
    return this.#rows.findIndex((row) => row.id === this.#selectedId)
  }

  get rowCount(): number {
    return this.#rows.length
  }

  open(
    presentation: ListDetailPresentation<Action>,
    onSelect: (action: Action) => void,
    handlers: Omit<ListDetailHandlers<Action>, "onSelect"> = {},
  ): void {
    this.#handlers = { onSelect, ...handlers }
    this.input.value = presentation.query
    this.visible = true
    this.refresh(presentation)
    this.input.focus()
  }

  refresh(presentation: ListDetailPresentation<Action>): void {
    const previousSelectedId = this.#selectedId
    this.heading.content = presentation.title
    if (this.input.value !== presentation.query) this.input.value = presentation.query
    this.#rows = presentation.rows
    this.#emptyCopy = presentation.emptyCopy
    if (!this.#rows.some((row) => row.kind === "item" && row.id === this.#pressedRowId)) {
      this.#pressedRowId = null
    }
    this.#selectedId = this.#retainedSelection(presentation.selectedId)
    this.#scrollOffset = Math.min(this.#scrollOffset, this.#maximumScrollOffset())
    this.#ensureSelectionVisible()
    this.#renderRows()
    this.#renderDetail()
    const notice = presentation.notice
    this.footer.fg = notice?.tone === "error"
      ? this.#theme.error
      : notice?.tone === "warning"
        ? this.#theme.warning
        : this.#theme.textMuted
    this.footer.content = notice === null || notice === undefined
      ? presentation.status
      : `${presentation.status} · ${notice.message}`
    if (previousSelectedId !== this.#selectedId) {
      this.#handlers?.onSelection?.(this.#selectedId)
    }
  }

  close(): void {
    this.#handlers = null
    this.visible = false
    this.input.blur()
    this.input.value = ""
    this.#rows = []
    this.#selectedId = null
    this.#scrollOffset = 0
    this.#pressedRowId = null
    this.#renderRows()
    this.detail.content = ""
    this.compactDetail.content = ""
  }

  resizeForTerminal(
    terminalWidth: number,
    terminalHeight: number,
    primaryHeight?: number,
  ): void {
    const primarySurface = this.#options.surfaceLayout === "primary"
    const inset = primarySurface ? 0 : terminalWidth >= 60 ? 1 : 0
    const width = primarySurface
      ? Math.max(1, terminalWidth)
      : Math.max(1, terminalWidth - inset * 2)
    const top = primarySurface ? 0 : terminalHeight >= 14 ? 2 : 0
    const height = primarySurface
      ? Math.max(6, Math.min(terminalHeight, primaryHeight ?? terminalHeight - 5))
      : Math.max(6, Math.min(25, terminalHeight - top - 5))
    this.left = inset
    this.top = top
    this.width = width
    this.height = height

    const horizontalPadding = primarySurface ? 1 : 2
    const innerWidth = Math.max(1, width - horizontalPadding * 2)
    this.#layoutMode = width >= (this.#options.splitMinWidth ?? 78) && height >= 10
      ? "split"
      : "single"
    const requestedListWidth = this.#options.splitListWidth ?? Math.floor(innerWidth / 2)
    const listWidth = this.#layoutMode === "split"
      ? Math.min(Math.max(1, requestedListWidth), Math.max(1, innerWidth - 2))
      : innerWidth
    const detailWidth = this.#layoutMode === "split" ? innerWidth - listWidth - 1 : 0
    const hasCompactDetail =
      this.#layoutMode === "single" &&
      height >= (this.#options.compactMinHeight ?? 10) &&
      this.#options.showCompactDetail !== false
    this.#visibleRows = Math.max(1, height - 5 - (hasCompactDetail ? 1 : 0))
    this.#listWidth = listWidth

    const leftContentWidth = primarySurface && this.#layoutMode === "split"
      ? listWidth
      : innerWidth
    this.heading.left = horizontalPadding
    this.heading.top = 0
    this.heading.width = leftContentWidth
    this.input.left = horizontalPadding
    this.input.top = 1
    this.input.width = leftContentWidth
    this.rule.left = horizontalPadding
    this.rule.top = 2
    this.rule.width = leftContentWidth
    this.rule.content = "─".repeat(leftContentWidth)
    this.listPane.left = horizontalPadding
    this.listPane.top = 3
    this.listPane.width = listWidth
    this.listPane.height = this.#visibleRows
    this.divider.left = horizontalPadding + listWidth
    this.divider.top = primarySurface ? 0 : 3
    this.divider.width = 1
    this.divider.height = primarySurface ? height : this.#visibleRows
    this.divider.content = Array.from(
      { length: primarySurface ? height : this.#visibleRows },
      () => "│",
    ).join("\n")
    this.divider.visible = this.#layoutMode === "split"
    this.detailPane.left = horizontalPadding + listWidth + 1
    this.detailPane.top = primarySurface ? 0 : 3
    this.detailPane.width = detailWidth
    this.detailPane.height = primarySurface ? height : this.#visibleRows
    this.detailPane.visible = this.#layoutMode === "split"
    this.compactDetail.left = horizontalPadding
    this.compactDetail.top = 3 + this.#visibleRows
    this.compactDetail.width = innerWidth
    this.compactDetail.visible = hasCompactDetail
    this.footer.left = horizontalPadding
    this.footer.top = height - 2
    this.footer.width = leftContentWidth
    this.#ensureRowViews()
    this.#scrollOffset = Math.min(this.#scrollOffset, this.#maximumScrollOffset())
    this.#renderRows()
    this.#renderDetail()
  }

  moveSelection(delta: number, wrap = true): void {
    const selectable = this.#selectableRows()
    if (selectable.length === 0) return
    const current = selectable.findIndex(({ row }) => row.id === this.#selectedId)
    const origin = current >= 0 ? current : 0
    let target = origin + delta
    if (wrap) target = ((target % selectable.length) + selectable.length) % selectable.length
    else target = Math.min(Math.max(target, 0), selectable.length - 1)
    this.#select(selectable[target]!.row.id, "center")
  }

  moveToBoundary(end: boolean): void {
    const selectable = this.#selectableRows()
    const target = end ? selectable.at(-1) : selectable[0]
    if (target === undefined) return
    this.#select(target.row.id, end ? "end" : "start")
  }

  scrollViewport(delta: number): void {
    const next = Math.min(
      this.#maximumScrollOffset(),
      Math.max(0, this.#scrollOffset + delta),
    )
    if (next === this.#scrollOffset) return
    this.#scrollOffset = next
    this.#renderRows()
  }

  selectById(id: string): void {
    this.#select(id)
  }

  restoreViewport(offset: number): void {
    this.#scrollOffset = Math.min(this.#maximumScrollOffset(), Math.max(0, Math.floor(offset)))
    this.#renderRows()
  }

  activateSelected(): boolean {
    const selected = this.#rows.find(
      (row): row is ListDetailItemRow<Action> => row.kind === "item" && row.id === this.#selectedId,
    )
    if (selected === undefined || this.#handlers === null) return false
    this.#handlers.onSelect(selected.action)
    return true
  }

  override destroy(): void {
    this.ctx.keyInput.off("keypress", this.#onKey)
    super.destroy()
  }

  #select(id: string, scroll: "center" | "start" | "end" | "edge" = "edge"): void {
    if (this.#rows.find((row) => row.kind === "item" && row.id === id) === undefined) return
    const changed = this.#selectedId !== id
    this.#selectedId = id
    const rowIndex = this.#rows.findIndex((row) => row.id === id)
    if (scroll === "center") {
      this.#scrollOffset = Math.min(
        this.#maximumScrollOffset(),
        Math.max(0, rowIndex - Math.floor(this.#visibleRows / 2)),
      )
    } else if (scroll === "start") {
      this.#scrollOffset = 0
    } else if (scroll === "end") {
      this.#scrollOffset = this.#maximumScrollOffset()
    } else {
      this.#ensureSelectionVisible()
    }
    this.#renderRows()
    this.#renderDetail()
    if (changed) this.#handlers?.onSelection?.(id)
  }

  #ensureSelectionVisible(): void {
    const index = this.#rows.findIndex((row) => row.id === this.#selectedId)
    if (index < 0) return
    if (index < this.#scrollOffset) this.#scrollOffset = index
    else if (index >= this.#scrollOffset + this.#visibleRows) {
      this.#scrollOffset = index - this.#visibleRows + 1
    }
  }

  #retainedSelection(requested: string | null): string | null {
    if (requested !== null && this.#rows.some((row) => row.kind === "item" && row.id === requested)) {
      return requested
    }
    return this.#rows.find((row): row is ListDetailItemRow<Action> => row.kind === "item")?.id ?? null
  }

  #selectableRows(): readonly { readonly row: ListDetailItemRow<Action>; readonly index: number }[] {
    return this.#rows.flatMap((row, index) => row.kind === "item" ? [{ row, index }] : [])
  }

  #maximumScrollOffset(): number {
    return Math.max(0, this.#rows.length - this.#visibleRows)
  }

  #ensureRowViews(): void {
    while (this.rowViews.length < this.#visibleRows) {
      const slot = this.rowViews.length
      const view = new TextRenderable(this.ctx, {
        id: `list-detail-row-${slot}`,
        position: "absolute",
        left: 0,
        top: slot,
        width: "100%",
        height: 1,
        fg: this.#theme.text,
        truncate: true,
      })
      view.onMouseDown = (event) => {
        if (event.button !== 0) return
        const row = this.#rows[this.#scrollOffset + slot]
        this.#pressedRowId = row?.kind === "item" ? row.id : null
        if (row?.kind === "item") {
          this.#select(row.id)
        }
        event.preventDefault()
        event.stopPropagation()
      }
      view.onMouseUp = (event) => {
        if (event.button !== 0) return
        const row = this.#rows[this.#scrollOffset + slot]
        const activate = row?.kind === "item" && row.id === this.#pressedRowId
        this.#pressedRowId = null
        if (activate) {
          this.#select(row.id)
          this.activateSelected()
        }
        event.preventDefault()
        event.stopPropagation()
      }
      view.onMouseScroll = this.listPane.onMouseScroll
      this.rowViews.push(view)
      this.listPane.add(view)
    }
    for (let index = 0; index < this.rowViews.length; index += 1) {
      this.rowViews[index]!.visible = index < this.#visibleRows
    }
  }

  #renderRows(): void {
    for (let slot = 0; slot < this.rowViews.length; slot += 1) {
      const view = this.rowViews[slot]!
      if (slot >= this.#visibleRows) {
        view.visible = false
        continue
      }
      const row = this.#rows[this.#scrollOffset + slot]
      view.visible = true
      view.top = slot
      if (row === undefined) {
        view.content = ""
        view.bg = this.#surfaceBackground
      } else if (row.kind === "section") {
        view.content = new StyledText([bold(fg(this.#theme.textMuted)(truncateToCells(row.label.toLocaleUpperCase(), Math.max(0, this.#listWidth - 2))))])
        view.bg = this.#surfaceBackground
      } else {
        const selected = row.id === this.#selectedId
        view.content = this.#options.renderRow?.(
          row,
          selected,
          this.#listWidth,
        ) ?? styledLabel(
          truncateToCells(row.label, Math.max(0, this.#listWidth - 2)),
          row.matchSpans,
          selected,
          this.#theme,
        )
        view.bg = selected ? this.#theme.backgroundPanel : this.#surfaceBackground
      }
    }
  }

  #renderDetail(): void {
    const selected = this.#rows.find(
      (row): row is ListDetailItemRow<Action> => row.kind === "item" && row.id === this.#selectedId,
    )
    if (selected === undefined) {
      const emptyCopy = this.#emptyCopy ?? this.#options.emptyCopy ?? "No matching commands"
      this.detail.content = emptyCopy
      this.compactDetail.content = emptyCopy
      return
    }
    this.detail.content = this.#options.renderDetail?.(
      selected,
      this.detailPane.width,
      this.detailPane.height,
    ) ?? new StyledText([
        bold(fg(this.#theme.text)(selected.detail.title)),
        fg(this.#theme.textMuted)(`\n${selected.detail.meta}\n\n`),
        fg(this.#theme.text)(selected.detail.description),
      ])
    this.compactDetail.content = selected.detail.description
  }
}

function styledLabel(
  label: string,
  matchSpans: readonly (readonly [number, number])[],
  selected: boolean,
  theme: RottweilerTheme,
): StyledText {
  const chunks: StyledText["chunks"] = [fg(selected ? theme.primary : theme.textMuted)(selected ? "› " : "  ")]
  let cursor = 0
  for (const [rawStart, rawEnd] of matchSpans) {
    const start = Math.min(label.length, Math.max(cursor, rawStart))
    const end = Math.min(label.length, Math.max(start, rawEnd))
    if (start > cursor) chunks.push(fg(theme.text)(label.slice(cursor, start)))
    if (end > start) chunks.push(bold(fg(theme.primary)(label.slice(start, end))))
    cursor = end
  }
  if (cursor < label.length) chunks.push(fg(theme.text)(label.slice(cursor)))
  return new StyledText(chunks)
}
