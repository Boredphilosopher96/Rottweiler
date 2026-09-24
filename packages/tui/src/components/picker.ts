import { StyledText, fg, type KeyEvent, type RenderContext } from "@opentui/core"

import { keyStrokeFromEvent } from "../keybindings"
import type { RottweilerTheme } from "../theme"
import {
  ListDetailRenderable,
  type ListDetailItemRow,
  type ListDetailPresentation,
  type ListDetailRow,
  type ListDetailTone,
} from "./list-detail"

export interface PickerItem<T> {
  readonly id: string
  readonly label: string
  readonly description: string
  readonly value: T
  readonly searchText?: string
  /** Render this row as status/context: it can be inspected but never activated. */
  readonly selectable?: boolean
  /** Empty-query grouping label; hidden as soon as fuzzy filtering starts. */
  readonly sectionHeader?: boolean
  /** Right-aligned secondary text in the row, repeated as the detail meta line. */
  readonly hint?: string
  /** Status glyph before the label, such as `●` for the current choice. */
  readonly marker?: string
  readonly tone?: ListDetailTone
  /** Detail pane body; defaults to `description`. */
  readonly detail?: string
  /** Enter's footer label on this row; `null` when Enter does nothing here. */
  readonly primary?: string | null
}

/** One screen-specific chord, listed in the footer while it applies. */
export interface PickerKey<T> {
  /** Canonical stroke, e.g. `ctrl+r`. */
  readonly stroke: string
  readonly label: string
  readonly run: (item: PickerItem<T> | null) => void
  /** The chord is hidden and inert while this returns false for the selection. */
  readonly available?: (item: PickerItem<T> | null) => boolean
}

export interface PickerScreenOptions<T> {
  /** Replaces the title line, e.g. with an inline usage meter. */
  readonly heading?: StyledText
  /** Enter's footer label unless a row overrides it; defaults to `select`. */
  readonly primary?: string
  readonly keys?: readonly PickerKey<T>[]
  /** Esc returns to the parent screen instead of closing. */
  readonly back?: () => void
  /** List copy when there are no rows. */
  readonly emptyCopy?: string
  readonly notice?: ListDetailPresentation<unknown>["notice"]
  /** Initial selection for a newly presented list. */
  readonly selectedId?: string | null
  /** Distinguishes nested views that share one screen kind; a new view starts fresh. */
  readonly view?: string
}

export interface TextPromptOptions {
  readonly title: string
  readonly placeholder: string
  readonly onSubmit: (value: string) => void
  readonly maxBytes: number
  readonly empty: "allow" | "reject"
  /** Guidance shown below the field. */
  readonly detail?: string
  /** Editable starting value, e.g. a suggested pattern the user reviews. */
  readonly initial?: string
}

type ScreenMode = "list" | "prompt" | "secret" | "status"

/**
 * Every generic navigation screen: the shared list-detail anatomy (title,
 * filter, grouped list, detail, state-aware footer) plus the text-entry and
 * status states a screen passes through. Anchored mode is the composer's `@`
 * mention list: the same rows, directly above the composer, which keeps focus.
 */
export class PickerScreenRenderable<T> extends ListDetailRenderable<PickerItem<T>> {
  #mode: ScreenMode = "list"
  #anchored = false
  #screen: string | null = null
  #title = ""
  #items: readonly PickerItem<T>[] = []
  #visible: readonly PickerItem<T>[] = []
  #onPick: ((item: PickerItem<T>) => void) | undefined
  #screenOptions: PickerScreenOptions<T> = {}
  #query = ""
  #desiredInlineRows = 3
  #secretValue = ""
  #textMaxBytes = 2048
  #textAllowsEmpty = false
  #onTextSubmit: ((value: string) => void) | undefined
  readonly #queryMaxLength: number
  readonly #chrome: RottweilerTheme
  readonly #onQuery: ((query: string) => void) | undefined
  /** Vim bindings need a second Esc: the first leaves insert mode. */
  vim = false
  #onPaste = (event: { bytes: Uint8Array; preventDefault(): void; stopPropagation(): void }) => {
    if (!this.visible || this.#anchored) return
    if (this.#mode === "status") {
      event.preventDefault()
      event.stopPropagation()
      return
    }
    if (this.#mode !== "prompt" && this.#mode !== "secret") return
    event.preventDefault()
    event.stopPropagation()
    let pasted: string
    try {
      pasted = new TextDecoder("utf-8", { fatal: true }).decode(event.bytes)
    } catch {
      return
    }
    if (!isPrintableInput(pasted)) return
    if (this.#mode === "prompt") {
      const candidate = this.input.value + pasted
      if (Buffer.byteLength(candidate) <= this.#textMaxBytes) this.input.value = candidate
      return
    }
    if (Buffer.byteLength(this.#secretValue + pasted) <= 8 * 1024) this.#secretValue += pasted
    this.#renderSecretMask()
  }

  constructor(ctx: RenderContext, theme: RottweilerTheme, onQuery?: (query: string) => void) {
    super(ctx, theme, {
      surfaceLayout: "primary",
      splitMinWidth: 90,
      surfaceBackground: theme.background,
      inputPlaceholder: "Type to filter…",
      emptyInList: true,
      fullWidthFooter: true,
    })
    this.#chrome = theme
    this.#onQuery = onQuery
    this.#queryMaxLength = this.input.maxLength
    ctx.keyInput.on("paste", this.#onPaste)
  }

  get anchored(): boolean { return this.#anchored }
  get mode(): ScreenMode { return this.#mode }
  get screenTitle(): string { return this.#title }
  /** Rows currently listed after filtering, excluding section headers. */
  get items(): readonly PickerItem<T>[] { return this.#visible.filter(item => item.sectionHeader !== true) }
  get selectedItem(): PickerItem<T> | null {
    return this.#visible.find(item => item.id === this.selectedId && item.sectionHeader !== true) ?? null
  }
  /** Rows a composer-anchored list wants, before the terminal constrains it. */
  get desiredInlineRows(): number { return this.#desiredInlineRows }

  /** The next presentation starts a new screen: fresh query and selection. */
  beginScreen(query: string): void {
    this.#screen = null
    this.#query = query
    this.input.value = query
  }

  /** Present or refresh a list. Refreshing the same screen keeps query and selection. */
  present(
    screen: string,
    title: string,
    items: readonly PickerItem<T>[],
    onPick: (item: PickerItem<T>) => void,
    options: PickerScreenOptions<T>,
    anchored: boolean,
    anchoredQuery: string,
  ): void {
    const fresh = !this.visible || this.#mode !== "list" || this.#screen !== screen || this.#anchored !== anchored
    this.#clearInputModes()
    this.#mode = "list"
    this.#screen = screen
    this.#anchored = anchored
    this.#title = title
    this.#items = items
    this.#onPick = onPick
    this.#screenOptions = options
    if (anchored) {
      const queryChanged = anchoredQuery !== this.#query
      this.#query = anchoredQuery
      this.#show(fresh || queryChanged)
      return
    }
    if (fresh) this.#query = this.input.value
    this.#show(fresh)
    if (fresh) this.input.focus()
  }

  openTextPrompt({ title, placeholder, onSubmit, maxBytes, empty, detail, initial }: TextPromptOptions, back?: () => void): void {
    this.#enterInputMode("prompt", title, back)
    if (initial !== undefined) this.input.value = initial
    this.#textAllowsEmpty = empty === "allow"
    this.#onTextSubmit = onSubmit
    this.#textMaxBytes = Math.max(1, Math.min(maxBytes, 8192))
    this.input.maxLength = this.#textMaxBytes
    this.input.placeholder = placeholder
    this.#openEmpty(title, detail ?? "", `⏎ ${empty === "allow" ? "continue" : "save"} · ${this.#escape(back)}`)
    this.input.focus()
  }

  openSecret(title: string, onSubmit: (secret: string) => void, back?: () => void): void {
    this.#enterInputMode("secret", title, back)
    this.#onTextSubmit = onSubmit
    this.input.placeholder = "API key (hidden)"
    this.#openEmpty(title, "The key is stored through the secure credential channel and never shown.", `⏎ save · ${this.#escape(back)}`)
    this.input.focus()
  }

  /** Transient or empty state; never presented as a selectable row. */
  showStatus(title: string, message: string, description = "", anchored = false): void {
    this.#clearInputModes()
    this.#mode = "status"
    this.#screen = null
    this.#anchored = anchored
    this.#title = title
    this.#items = []
    this.#visible = []
    this.#onPick = undefined
    this.#screenOptions = {}
    this.#desiredInlineRows = description.length === 0 ? 2 : 3
    this.#openEmpty(title, description.length === 0 ? message : `${message}\n${description}`, this.#escape(undefined))
    this.input.blur()
    this.input.visible = false
  }

  showLoading(title: string, message: string, anchored = false): void {
    this.showStatus(title, `◌ ${message}`, "This screen updates automatically.", anchored)
  }

  /** Escape handler for the current screen, when it returns to a parent. */
  get back(): (() => void) | undefined { return this.#screenOptions.back }

  override close(): void {
    this.#clearInputModes()
    this.#mode = "list"
    this.#screen = null
    this.#anchored = false
    this.#items = []
    this.#visible = []
    this.#onPick = undefined
    this.#screenOptions = {}
    this.#query = ""
    this.input.placeholder = "Type to filter…"
    super.close()
    this.input.visible = true
  }

  override destroy(): void {
    this.ctx.keyInput.off("paste", this.#onPaste)
    this.#onPick = undefined
    this.#onTextSubmit = undefined
    super.destroy()
  }

  /** An anchored status line leaves Enter and arrows to the composer that owns focus. */
  protected override get navigable(): boolean { return !(this.#anchored && this.#mode === "status") }

  protected override interceptKey(key: KeyEvent): boolean {
    const plain = !key.ctrl && !key.meta && !key.option
    if (this.#mode === "status" && !this.#anchored) {
      // A status surface is not an action list; only Esc and global chords pass.
      return key.name !== "escape" && plain
    }
    if (this.#mode === "prompt" && plain) return this.#promptKey(key)
    if (this.#mode === "secret" && plain) return this.#secretKey(key)
    if (this.#mode !== "list") return false
    if (this.#anchored && plain && !key.shift && key.name === "tab") return this.activateSelected()
    const stroke = keyStrokeFromEvent(key)
    const selected = this.selectedItem
    const chord = this.#screenOptions.keys?.find(candidate =>
      candidate.stroke === stroke && (candidate.available?.(selected) ?? true))
    if (chord === undefined) return false
    chord.run(selected)
    return true
  }

  #promptKey(key: KeyEvent): boolean {
    if (key.name === "return" || key.name === "kpenter") {
      const value = this.input.value.trim()
      if (value.length > 0 || this.#textAllowsEmpty) {
        const submit = this.#onTextSubmit
        this.#clearInputModes()
        submit?.(value)
      }
    } else if (key.name === "backspace" || key.name === "delete") {
      this.input.value = Array.from(this.input.value).slice(0, -1).join("")
    } else if (isPrintableInput(key.sequence)) {
      const candidate = this.input.value + key.sequence
      if (Buffer.byteLength(candidate) <= this.#textMaxBytes) this.input.value = candidate
    } else {
      return false
    }
    return true
  }

  #secretKey(key: KeyEvent): boolean {
    if (key.name === "return" || key.name === "kpenter") {
      if (this.#secretValue.length > 0) {
        const secret = this.#secretValue
        const submit = this.#onTextSubmit
        this.#clearInputModes()
        submit?.(secret)
      }
    } else if (key.name === "backspace" || key.name === "delete") {
      this.#secretValue = Array.from(this.#secretValue).slice(0, -1).join("")
      this.#renderSecretMask()
    } else if (isPrintableInput(key.sequence)) {
      if (Buffer.byteLength(this.#secretValue + key.sequence) <= 8 * 1024) {
        this.#secretValue += key.sequence
        this.#renderSecretMask()
      }
    } else {
      return false
    }
    return true
  }

  #enterInputMode(mode: "prompt" | "secret", title: string, back: (() => void) | undefined): void {
    this.#clearInputModes()
    this.#mode = mode
    this.#screen = null
    this.#anchored = false
    this.#title = title
    this.#items = []
    this.#visible = []
    this.#onPick = undefined
    this.#screenOptions = back === undefined ? {} : { back }
  }

  #openEmpty(title: string, copy: string, status: string): void {
    const presentation: ListDetailPresentation<PickerItem<T>> = {
      title: this.#anchored ? this.#inlineHeading(title, status) : title,
      query: this.input.value,
      rows: [],
      selectedId: null,
      status,
      emptyCopy: copy,
    }
    this.#present(presentation)
    this.input.visible = !this.#anchored
  }

  #present(presentation: ListDetailPresentation<PickerItem<T>>): void {
    if (this.visible) {
      this.refresh(presentation)
      return
    }
    this.open(presentation, item => this.#activate(item), {
      onQuery: query => {
        if (this.#mode !== "list" || this.#anchored) return
        this.#show(false)
        this.#onQuery?.(query)
      },
      onSelection: () => {
        if (this.#mode === "list") this.footer.content = this.#footerWithNotice(this.#footer(this.selectedId))
      },
    })
  }

  #show(fresh: boolean): void {
    const query = this.#anchored ? this.#query : this.input.value
    this.#visible = filterItems(this.#items, query)
    const listed = this.#visible.filter(item => item.sectionHeader !== true)
    const firstActive = listed.find(item => item.selectable !== false)?.id ?? listed[0]?.id ?? null
    const requested = fresh || query !== this.#query
      ? (listed.some(item => item.id === this.#screenOptions.selectedId) && query.length === 0
          ? this.#screenOptions.selectedId! : firstActive)
      : this.selectedId ?? firstActive
    this.#query = query
    this.#desiredInlineRows = Math.min(12, Math.max(2, this.#visible.length + 1))
    const rows = this.#visible.map((item): ListDetailRow<PickerItem<T>> => item.sectionHeader === true
      ? { kind: "section", id: item.id, label: item.label }
      : itemRow(item, query))
    const status = this.#footer(requested)
    const presentation: ListDetailPresentation<PickerItem<T>> = {
      title: this.#anchored ? this.#inlineHeading(this.#title, status) : this.#screenOptions.heading ?? this.#title,
      query: this.#anchored ? "" : query,
      rows,
      selectedId: requested,
      status,
      emptyCopy: this.#screenOptions.emptyCopy
        ?? (query.trim().length > 0 ? `No matches for “${query.trim()}”` : "Nothing to show"),
      notice: this.#screenOptions.notice ?? null,
    }
    this.#present(presentation)
    this.input.visible = !this.#anchored
  }

  #activate(item: PickerItem<T>): void {
    if (this.#mode !== "list" || item.selectable === false || item.primary === null) return
    this.#onPick?.(item)
  }

  #footerWithNotice(status: string): string {
    const notice = this.#screenOptions.notice
    return notice === null || notice === undefined ? status : `${status} · ${notice.message}`
  }

  #footer(selectedId: string | null): string {
    const selected = this.#visible.find(item => item.id === selectedId && item.sectionHeader !== true) ?? null
    const primary = selected === null || selected.selectable === false
      ? null
      : selected.primary === undefined ? this.#screenOptions.primary ?? "select" : selected.primary
    const keys = (this.#screenOptions.keys ?? [])
      .filter(key => key.available?.(selected) ?? true)
      .map(key => `${key.stroke} ${key.label}`)
    return [
      ...(primary === null ? [] : [`${this.#anchored ? "⏎/tab" : "⏎"} ${primary}`]),
      ...keys,
      this.#escape(this.#screenOptions.back),
    ].join(" · ")
  }

  #escape(back: (() => void) | undefined): string {
    return `${this.vim && !this.#anchored ? "esc×2" : "esc"} ${back === undefined ? "close" : "back"}`
  }

  #inlineHeading(title: string, status: string): StyledText {
    return new StyledText([
      fg(this.#chrome.text)(title),
      fg(this.#chrome.textMuted)(`  ${status}`),
    ])
  }

  #renderSecretMask(): void {
    const length = Array.from(this.#secretValue).length
    this.input.value = `${"•".repeat(Math.min(length, 64))}${length > 64 ? "…" : ""}`
  }

  #clearInputModes(): void {
    const wasInput = this.#mode === "prompt" || this.#mode === "secret"
    this.#secretValue = ""
    this.#textAllowsEmpty = false
    this.#onTextSubmit = undefined
    this.#textMaxBytes = 2048
    this.input.maxLength = this.#queryMaxLength
    if (wasInput) {
      this.input.value = ""
      this.input.placeholder = "Type to filter…"
    }
  }
}

function itemRow<T>(item: PickerItem<T>, query: string): ListDetailItemRow<PickerItem<T>> {
  return {
    kind: "item",
    id: item.id,
    label: item.label,
    disabled: item.selectable === false,
    ...(item.hint === undefined ? {} : { hint: item.hint }),
    ...(item.marker === undefined ? {} : { marker: item.marker }),
    ...(item.tone === undefined ? {} : { tone: item.tone }),
    matchSpans: matchSpans(fuzzyMatch(query, item.label)),
    detail: {
      title: item.label,
      meta: item.hint ?? "",
      description: item.detail ?? item.description,
    },
    action: item,
  }
}

/** Empty queries keep screen order and sections; a query ranks matching rows. */
function filterItems<T>(items: readonly PickerItem<T>[], query: string): readonly PickerItem<T>[] {
  if (query.trim().length === 0) return items
  return items
    .filter(item => item.sectionHeader !== true)
    .map((item, index) => ({ item, index, score: pickerItemScore(query, item) }))
    .filter(entry => entry.score !== null)
    .sort((left, right) => (right.score ?? 0) - (left.score ?? 0) || left.index - right.index)
    .map(entry => entry.item)
}

function matchSpans(match: FuzzyMatch | null): readonly (readonly [number, number])[] {
  if (match === null) return []
  const spans: Array<readonly [number, number]> = []
  for (const position of match.positions) {
    const previous = spans.at(-1)
    if (previous !== undefined && previous[1] === position) spans[spans.length - 1] = [previous[0], position + 1]
    else spans.push([position, position + 1])
  }
  return spans
}

function pickerItemScore<T>(query: string, item: PickerItem<T>): number | null {
  const needle = query.trim().toLocaleLowerCase()
  if (needle.length === 0) return 0

  const label = item.label.toLocaleLowerCase()
  const searchText = (item.searchText ?? "").toLocaleLowerCase()
  const description = item.description.toLocaleLowerCase()
  const labelScore = fuzzyScore(needle, label)
  const searchScore = searchText.length === 0 ? null : fuzzyScore(needle, searchText)
  const descriptionScore = fuzzyScore(needle, description)
  const scores: number[] = []

  if (labelScore !== null) {
    const exact = label === needle || label === `/${needle}`
    const prefix = label.startsWith(needle) || label.startsWith(`/${needle}`)
    scores.push(labelScore + (exact ? 1_000 : prefix ? 500 : 200))
  }
  if (searchScore !== null) scores.push(searchScore + 100)
  if (descriptionScore !== null) scores.push(descriptionScore)
  return scores.length === 0 ? null : Math.max(...scores)
}

function isPrintableInput(value: string): boolean {
  return value.length > 0 && Array.from(value).every((character) => {
    const code = character.codePointAt(0) ?? 0
    return !(
      code < 0x20 ||
      (code >= 0x7f && code <= 0x9f) ||
      /[\p{Cf}\p{Zl}\p{Zp}]/u.test(character)
    )
  })
}

export function fuzzyScore(query: string, candidate: string): number | null {
  return fuzzyMatch(query, candidate)?.score ?? null
}

export interface FuzzyMatch {
  readonly score: number
  readonly positions: readonly number[]
}

export function fuzzyMatch(query: string, candidate: string): FuzzyMatch | null {
  const needle = query.trim().toLocaleLowerCase()
  const haystack = candidate.toLocaleLowerCase()
  if (needle.length === 0) {
    return { score: 0, positions: [] }
  }
  let cursor = 0
  let score = 0
  let streak = 0
  const positions: number[] = []
  for (let index = 0; index < haystack.length && cursor < needle.length; index += 1) {
    if (haystack[index] !== needle[cursor]) {
      streak = 0
      continue
    }
    streak += 1
    score += 10 + streak * 3 - Math.min(index, 20)
    if (index === 0 || /[\s/_.-]/.test(haystack[index - 1] ?? "")) {
      score += 12
    }
    positions.push(index)
    cursor += 1
  }
  return cursor === needle.length
    ? { score: score - (haystack.length - needle.length) * 0.05, positions }
    : null
}
