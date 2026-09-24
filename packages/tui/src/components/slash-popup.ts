import { BoxRenderable, StyledText, bold, fg, type KeyEvent, type RenderContext } from "@opentui/core"

import type { CommandEntry, CommandRow } from "../command-palette"
import { stringCellWidth, truncateToCells } from "../render/text"
import type { RottweilerTheme } from "../theme"
import { TextRenderable } from "./text"

const MAX_VISIBLE_ROWS = 10

export type SlashPopupModel<Action> =
  | {
      readonly kind: "commands"
      /** Typed name fragment; a new fragment selects its best match. */
      readonly query: string
      readonly rows: readonly CommandRow<Action>[]
      readonly selectedId: string | null
    }
  | {
      readonly kind: "arguments"
      readonly entry: CommandEntry<Action>
    }

export interface SlashPopupHandlers<Action> {
  /** Tab: complete the selected name in the composer. */
  readonly onComplete: (entry: CommandEntry<Action>) => void
  /** Enter: run the selected entry. */
  readonly onRun: (entry: CommandEntry<Action>) => void
  /** Esc while open: close and keep the composer text. */
  readonly onDismiss: () => void
  /** Esc directly after a dismissal: clear the composer text. */
  readonly onClear: () => void
  /** The popup owns keys only while the composer has focus. */
  readonly active: () => boolean
  /** Composer top row; the popup sits directly above it. */
  readonly anchorRow: () => number
}

/**
 * Composer-anchored slash completion. Text stays in the composer: this surface
 * only ranks, completes, and runs the command name being typed.
 */
export class SlashPopupRenderable<Action> extends BoxRenderable {
  readonly rowViews: TextRenderable[] = []
  #model: SlashPopupModel<Action> | null = null
  #selectedId: string | null = null
  #scroll = 0
  #clearArmed = false
  #visibleRows = 1
  #innerWidth = 10
  readonly #theme: RottweilerTheme
  readonly #handlers: SlashPopupHandlers<Action>
  readonly #onKey = (key: KeyEvent) => {
    const plain = !key.ctrl && !key.meta && !key.option && !key.shift
    if (!this.visible) {
      if (this.#clearArmed && plain && key.name === "escape" && this.#handlers.active()) {
        this.#clearArmed = false
        this.#handlers.onClear()
        key.preventDefault()
        key.stopPropagation()
        return
      }
      this.#clearArmed = false
      return
    }
    if (!this.#handlers.active()) return
    const model = this.#model
    let handled = true
    if (plain && key.name === "escape") {
      this.#clearArmed = true
      this.#handlers.onDismiss()
    } else if (model?.kind !== "commands") {
      handled = false
    } else if (plain && key.name === "up") {
      this.moveSelection(-1)
    } else if (plain && key.name === "down") {
      this.moveSelection(1)
    } else if (plain && key.name === "tab" && this.selected !== null) {
      this.#handlers.onComplete(this.selected)
    } else if (plain && (key.name === "return" || key.name === "kpenter") && this.selected !== null) {
      this.#handlers.onRun(this.selected)
    } else {
      handled = false
    }
    if (!handled) return
    key.preventDefault()
    key.stopPropagation()
  }

  constructor(ctx: RenderContext, theme: RottweilerTheme, handlers: SlashPopupHandlers<Action>) {
    super(ctx, {
      id: "slash-popup",
      position: "absolute",
      left: 1,
      visible: false,
      zIndex: 15,
      border: true,
      borderStyle: "rounded",
      borderColor: theme.borderSubtle,
      backgroundColor: theme.backgroundPanel,
      paddingX: 1,
      overflow: "hidden",
    })
    this.#theme = theme
    this.#handlers = handlers
    // Prepended so completion keys win over composer history and submit.
    ctx.keyInput.prependListener("keypress", this.#onKey)
  }

  get model(): SlashPopupModel<Action> | null { return this.#model }
  get selectedId(): string | null { return this.#selectedId }
  get itemIds(): readonly string[] {
    return this.#model?.kind === "commands"
      ? this.#model.rows.flatMap((row) => row.kind === "item" ? [row.id] : [])
      : []
  }
  get sectionLabels(): readonly string[] {
    return this.#model?.kind === "commands"
      ? this.#model.rows.flatMap((row) => row.kind === "section" ? [row.label] : [])
      : []
  }
  get selected(): CommandEntry<Action> | null {
    if (this.#model?.kind !== "commands") return null
    const row = this.#model.rows.find((candidate) => candidate.kind === "item" && candidate.id === this.#selectedId)
    return row?.kind === "item" && row.entry.unavailableReason === null ? row.entry : null
  }

  show(model: SlashPopupModel<Action>): void {
    const previous = this.#model
    this.#model = model
    this.#clearArmed = false
    if (model.kind === "commands") {
      const retained = previous?.kind === "commands" && previous.query === model.query && model.rows.some((row) =>
        row.kind === "item" && row.id === this.#selectedId && row.entry.unavailableReason === null)
      if (!retained) {
        this.#selectedId = model.selectedId
        this.#scroll = 0
      }
    }
    this.visible = true
    this.#layout()
  }

  hide(): void {
    if (!this.visible && this.#model === null) return
    this.visible = false
    this.#model = null
    this.#selectedId = null
    this.#scroll = 0
  }

  /** Forget a pending Esc-to-clear after the composer text changes. */
  disarmClear(): void { this.#clearArmed = false }

  moveSelection(delta: number): void {
    if (this.#model?.kind !== "commands") return
    const items = this.#model.rows.flatMap((row) =>
      row.kind === "item" && row.entry.unavailableReason === null ? [row.id] : [])
    if (items.length === 0) return
    const current = items.indexOf(this.#selectedId ?? "")
    const next = current < 0 ? 0 : (current + delta + items.length) % items.length
    this.#selectedId = items[next] ?? null
    this.#render()
  }

  override destroy(): void {
    this.ctx.keyInput.off("keypress", this.#onKey)
    super.destroy()
  }

  #layout(): void {
    const model = this.#model
    if (model === null) return
    const rows = model.kind === "commands" ? Math.min(MAX_VISIBLE_ROWS, Math.max(1, model.rows.length)) : 2
    const anchor = Math.max(0, this.#handlers.anchorRow())
    const height = Math.max(3, Math.min(rows + 2, anchor))
    const width = Math.max(10, this.ctx.width - 2)
    this.width = width
    this.height = height
    this.#visibleRows = height - 2
    this.#innerWidth = Math.max(8, width - 4)
    this.top = Math.max(0, anchor - height)
    while (this.rowViews.length < MAX_VISIBLE_ROWS) {
      const view = new TextRenderable(this.ctx, {
        id: `slash-popup-row-${this.rowViews.length}`,
        height: 1,
        width: "100%",
        truncate: true,
        fg: this.#theme.text,
      })
      this.rowViews.push(view)
      this.add(view)
    }
    this.#render()
  }

  #render(): void {
    const model = this.#model
    if (model === null) return
    const visibleRows = this.#visibleRows
    const width = this.#innerWidth
    if (model.kind === "arguments") {
      const entry = model.entry
      const usage = `/${entry.name}${entry.argumentHint.length === 0 ? "" : ` ${entry.argumentHint}`}`
      this.rowViews.forEach((view, index) => {
        view.visible = index < 2
        view.bg = this.#theme.backgroundPanel
      })
      this.rowViews[0]!.content = new StyledText([
        bold(fg(this.#theme.primary)(truncateToCells(usage, width))),
      ])
      this.rowViews[1]!.content = new StyledText([
        fg(this.#theme.textMuted)(truncateToCells(`${entry.description} · Enter to run`, width)),
      ])
      return
    }
    const index = model.rows.findIndex((row) => row.id === this.#selectedId)
    if (index >= 0 && index < this.#scroll) this.#scroll = index
    if (index >= this.#scroll + visibleRows) this.#scroll = index - visibleRows + 1
    this.#scroll = Math.max(0, Math.min(this.#scroll, model.rows.length - visibleRows))
    this.rowViews.forEach((view, slot) => {
      const row = model.rows[this.#scroll + slot]
      view.visible = slot < visibleRows
      const selected = row?.kind === "item" && row.id === this.#selectedId
      view.bg = selected ? this.#theme.backgroundElement : this.#theme.backgroundPanel
      view.content = row === undefined
        ? ""
        : row.kind === "section"
          ? new StyledText([bold(fg(this.#theme.textMuted)(row.label.toLocaleUpperCase()))])
          : slashRow(row.entry, selected, width, this.#theme)
    })
  }
}

/** `/name hint · description … trailing` with the trailing hint right-aligned. */
export function slashRow<Action>(
  entry: CommandEntry<Action>,
  selected: boolean,
  width: number,
  theme: RottweilerTheme,
): StyledText {
  const disabled = entry.unavailableReason !== null
  const trailing = disabled ? "unavailable" : entry.keycap ?? entry.sourceLabel ?? ""
  const marker = selected ? "› " : "  "
  const name = `/${entry.name}`
  const nameWidth = Math.min(Math.max(14, stringCellWidth(name) + 2), Math.floor(width / 2))
  const nameText = truncateToCells(name, nameWidth - 1).padEnd(nameWidth)
  const trailingWidth = trailing.length === 0 ? 0 : stringCellWidth(trailing) + 2
  const descriptionWidth = Math.max(0, width - 2 - nameWidth - trailingWidth)
  const description = truncateToCells(disabled ? entry.unavailableReason ?? "" : entry.description, descriptionWidth)
  const gap = " ".repeat(Math.max(0, descriptionWidth - stringCellWidth(description)) + (trailingWidth > 0 ? 2 : 0))
  const nameColor = disabled ? theme.textMuted : selected ? theme.primary : theme.text
  return new StyledText([
    fg(selected ? theme.primary : theme.textMuted)(marker),
    selected && !disabled ? bold(fg(nameColor)(nameText)) : fg(nameColor)(nameText),
    fg(theme.textMuted)(description),
    fg(theme.textMuted)(`${gap}${trailing}`),
  ])
}

/** Palette row: title with match highlights and a right-aligned key or source. */
export function paletteRow<Action>(
  entry: CommandEntry<Action>,
  matchSpans: readonly (readonly [number, number])[],
  selected: boolean,
  width: number,
  theme: RottweilerTheme,
): StyledText {
  // Keep one cell clear of the split divider.
  width -= 1
  const disabled = entry.unavailableReason !== null
  const trailing = disabled ? "unavailable" : entry.keycap ?? entry.sourceLabel ?? ""
  const trailingWidth = trailing.length === 0 ? 0 : stringCellWidth(trailing) + 2
  const label = truncateToCells(entry.title, Math.max(1, width - 2 - trailingWidth))
  const chunks: StyledText["chunks"] = [fg(selected ? theme.primary : theme.textMuted)(selected ? "› " : "  ")]
  const base = disabled ? theme.textMuted : theme.text
  let cursor = 0
  for (const [rawStart, rawEnd] of disabled ? [] : matchSpans) {
    const start = Math.min(label.length, Math.max(cursor, rawStart))
    const end = Math.min(label.length, Math.max(start, rawEnd))
    if (start > cursor) chunks.push(fg(base)(label.slice(cursor, start)))
    if (end > start) chunks.push(bold(fg(theme.primary)(label.slice(start, end))))
    cursor = end
  }
  if (cursor < label.length) chunks.push(fg(base)(label.slice(cursor)))
  const gap = Math.max(1, width - 2 - stringCellWidth(label) - stringCellWidth(trailing))
  if (trailing.length > 0) chunks.push(fg(theme.textMuted)(`${" ".repeat(gap)}${trailing}`))
  return new StyledText(chunks)
}
