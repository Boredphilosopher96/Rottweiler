import { StyledText, bg, bold, fg } from "@opentui/core"
import type { ListDetailItemRow } from "../components"
import { ListDetailRenderable, type ListDetailPresentation } from "../components"
import { PickerController } from "../picker-controller"
import { ProjectionRequestBroker } from "../projection-requests"
import { type CommandOutcome } from "../protocol"
import { stringCellWidth, truncateToCells } from "../render"
import { THEME_ROLE_KEYS, themeCatalog, type RottweilerTheme } from "../theme"
import { createThemeBrowserModel } from "../theme-browser"

interface ThemeUiHost {
  readonly theme: RottweilerTheme
  readonly browser: ListDetailRenderable<RottweilerTheme>
  readonly pickerController: PickerController
  readonly requests: ProjectionRequestBroker
  readonly sessionId: string
  readonly terminalWidth: number
  readonly terminalHeight: number
  readonly statusHeight: number
  readonly composerDockHeight: number
  readonly vim: boolean
  readonly deferred: boolean
  readonly previewSuppressed: boolean
  resolveTheme(theme: RottweilerTheme): RottweilerTheme
  applyTheme(theme: RottweilerTheme): void
  withPreviewSuppressed(action: () => void): void
  closePicker(): void
  modalOpened(): void
  projectRejection(outcome: Extract<CommandOutcome, { type: "rejected" }>): void
  projectError(code: string, message: string, retryable?: boolean): void
}
export class ThemeUiController {
  readonly #host: ThemeUiHost
  #pending: object | null = null
  #themeBeforePreview: RottweilerTheme | null = null
  #themePreviewCommitted = false
  constructor(host: ThemeUiHost) { this.#host = host }
  get previewBase(): RottweilerTheme | null { return this.#themeBeforePreview }
  restorePreviewBase(theme: RottweilerTheme | null): void { this.#themeBeforePreview = theme }
  dispose(): void {
    this.#pending = null
    this.#themeBeforePreview = null
    this.#themePreviewCommitted = false
  }
  #finishPreview(): void {
    const captured = !this.#themePreviewCommitted ? this.#themeBeforePreview : null
    this.dispose()
    if (captured === null) return
    const restored = this.#host.resolveTheme(captured)
    if (restored.name !== this.#host.theme.name || this.#host.deferred) this.#host.applyTheme(restored)
  }
  openThemePicker(): void {
    this.#host.pickerController.begin("themes")
    this.#pending = null
    this.#themeBeforePreview = this.#host.theme
    this.#themePreviewCommitted = false
    this.#host.pickerController.interaction?.onRetire(() => this.#finishPreview())
    this.resize(
      this.#host.terminalWidth,
      this.#host.terminalHeight,
    )
    this.#host.withPreviewSuppressed(() => this.#host.pickerController.refresh())
    this.#host.browser.input.focus()
  }

  #previewTheme(theme: RottweilerTheme): void {
    if (theme.name === this.#host.theme.name && !this.#host.deferred) return
    this.#host.applyTheme(theme)
  }

  resize(width: number, height: number): void {
    const primaryHeight = Math.max(
      6,
      height - this.#host.statusHeight - this.#host.composerDockHeight,
    )
    this.#host.browser.resizeForTerminal(width, height, primaryHeight)
  }

  async #confirmTheme(theme: RottweilerTheme): Promise<void> {
    const interaction = this.#host.pickerController.interaction
    if (!interaction?.active || this.#pending !== null) return
    const pending = this.#pending = {}
    this.#host.pickerController.refresh()
    try {
      const outcome = await this.#host.requests.emit({
        type: "set_setting", meta: this.#host.requests.meta(), session_id: this.#host.sessionId,
        key: "ui.theme", value: theme.name,
      })
      if (!interaction.active || this.#pending !== pending) return
      this.#pending = null
      if (outcome?.type !== "accepted") {
        if (outcome?.type === "rejected") this.#host.projectRejection(outcome)
        else this.#host.projectError("theme_persistence_failed", "theme could not be persisted", true)
        this.#host.closePicker()
        return
      }
      this.#themePreviewCommitted = true
      this.#themeBeforePreview = theme
      this.#previewTheme(theme)
      this.#host.closePicker()
    } catch {
      if (!interaction.active || this.#pending !== pending) return
      this.#pending = null
      this.#host.projectError("theme_persistence_failed", "theme could not be persisted", true)
      this.#host.closePicker()
    }
  }
  render(): void {
    const interaction = this.#host.pickerController.interaction
    this.resize(
      this.#host.terminalWidth,
      this.#host.terminalHeight,
    )
    const themes = themeCatalog.map((catalogTheme) => this.#host.resolveTheme(catalogTheme))
    const query = this.#host.browser.visible
      ? this.#host.browser.input.value
      : this.#host.pickerController.query
    const preserveSelection = query === this.#host.pickerController.query
    this.#host.pickerController.query = query
    const selectedId = this.#host.browser.visible && preserveSelection
      ? this.#host.browser.selectedId
      : null
    const model = createThemeBrowserModel({
      themes,
      query,
      selectedName: selectedId?.slice("theme:".length) ?? null,
      currentName: this.#host.theme.name,
    })
    const presentation: ListDetailPresentation<RottweilerTheme> = {
      title: `${model.title}   ${model.counts.total} themes   /theme`,
      query,
      selectedId: model.selectedId,
      rows: model.rows.map((row) => ({
        kind: "item",
        id: row.id,
        label: row.name,
        matchSpans: row.matchSpans,
        detail: {
          title: row.name,
          description: "custom ~/.rottweiler/themes/ · data only, never executed",
          meta: `${row.mode} · ${row.source}`,
        },
        action: row.theme,
      })),
      status: this.#pending !== null ? "Saving theme… · esc cancel" : this.#host.vim
        ? "↑↓ preview  ⏎ apply  esc×2 cancel"
        : "↑↓ preview · ⏎ apply · esc cancel",
    }
    if (this.#host.browser.visible) {
      this.#host.browser.refresh(presentation)
    } else {
      this.#host.browser.open(presentation, (theme) => {
        if (interaction?.active) void this.#confirmTheme(theme)
      }, {
        onQuery: () => { if (interaction?.active) this.#host.pickerController.refresh() },
        onSelection: (id) => {
          if (!interaction?.active || this.#pending !== null || this.#host.previewSuppressed || id === null) return
          const selectedTheme = themes.find((theme) => `theme:${theme.name}` === id)
          if (selectedTheme === undefined) return
          if (this.#themeBeforePreview === null) this.#themeBeforePreview = this.#host.theme
          this.#previewTheme(selectedTheme)
        },
      })
      this.#host.modalOpened()
    }
  }
}

export function themeBrowserRow(
  row: ListDetailItemRow<RottweilerTheme>,
  selected: boolean,
  availableWidth: number,
  chromeTheme: RottweilerTheme,
): StyledText {
  const theme = row.action
  const pointer = selected ? "› " : "  "
  const tag = theme.name === "system" ? " ansi" : selected ? ` ${theme.mode}` : ""
  const swatchColors = [
    theme.background,
    theme.primary,
    theme.accent,
    theme.success,
    theme.error,
  ] as const
  const fixedWidth = stringCellWidth(pointer) + stringCellWidth(tag)
  const swatchCount = Math.min(
    swatchColors.length,
    Math.max(0, Math.floor((availableWidth - fixedWidth - 7) / 2)),
  )
  const swatchWidth = swatchCount * 2
  const nameWidth = Math.max(0, availableWidth - fixedWidth - swatchWidth - 1)
  const name = truncateToCells(row.label, nameWidth)
  const gap = " ".repeat(Math.max(1, nameWidth - stringCellWidth(name) + 1))
  const nameChunk = fg(selected ? chromeTheme.selectedListItemText : chromeTheme.text)(name)
  const chunks: StyledText["chunks"] = [
    fg(selected ? chromeTheme.primary : chromeTheme.textMuted)(pointer),
    selected ? bold(nameChunk) : nameChunk,
    fg(chromeTheme.textMuted)(gap),
  ]
  for (const color of swatchColors.slice(0, swatchCount)) {
    chunks.push(fg(color)("██"))
  }
  if (tag.length > 0) chunks.push(fg(chromeTheme.textMuted)(tag))
  return new StyledText(chunks)
}

export function themeBrowserDetail(theme: RottweilerTheme): StyledText {
  return new StyledText([
    bold(fg(theme.primary)(`${theme.name}  `)),
    fg(theme.textMuted)(`${theme.mode} · ${THEME_ROLE_KEYS.length} roles resolved · live sample\n`),
    fg(theme.borderSubtle)("────────────────────────────────────────────────────────────\n"),
    bold(fg(theme.primary)("you       ")),
    fg(theme.markdownText)("Make the picker feel deliberate.\n"),
    bold(fg(theme.accent)("assistant ")),
    fg(theme.text)("The layout stays fixed; roles supply the color.\n"),
    fg(theme.textMuted)("reasoning  Keep hierarchy quiet and readable.\n"),
    bold(fg(theme.markdownHeading)("Markdown roles\n")),
    fg(theme.markdownText)("text  "),
    fg(theme.markdownCode)("`inline code`  "),
    fg(theme.markdownLink)("link\n"),
    fg(theme.markdownBlockQuote)("│ quoted context stays secondary\n"),
    bold(fg(theme.syntaxFunction)("function ")),
    fg(theme.syntaxVariable)("applyTheme"),
    fg(theme.syntaxPunctuation)("("),
    fg(theme.syntaxVariable)("name"),
    fg(theme.syntaxPunctuation)(") {\n"),
    fg(theme.syntaxComment)("  // roles change; geometry does not\n"),
    fg(theme.syntaxKeyword)("  const "),
    fg(theme.syntaxVariable)("active"),
    fg(theme.syntaxOperator)(" = "),
    fg(theme.syntaxString)("name"),
    fg(theme.syntaxPunctuation)("\n}\n"),
    bg(theme.diffRemovedBg)(fg(theme.diffRemoved)("− removed line")),
    fg(theme.text)("\n"),
    bg(theme.diffAddedBg)(fg(theme.diffAdded)("+ added line")),
    fg(theme.text)("\n"),
    bold(fg(theme.success)("success  ")),
    bold(fg(theme.warning)("warning  ")),
    bold(fg(theme.error)("error\n")),
    fg(theme.textMuted)("background / panel / element  "),
    fg(theme.text)(`${theme.background}  ${theme.backgroundPanel}  ${theme.backgroundElement}\n`),
    fg(theme.textMuted)("border / text / muted          "),
    fg(theme.text)(`${theme.border}  ${theme.text}  ${theme.textMuted}\n`),
    fg(theme.textMuted)("Themes change semantic roles, not layout.\n"),
    fg(theme.textMuted)("Every sample above is rendered from the selected theme.\n"),
    fg(theme.textMuted)("custom "),
    fg(theme.success)("~/.rottweiler/themes/\n"),
    fg(theme.textMuted)("data only, never executed"),
  ])
}
