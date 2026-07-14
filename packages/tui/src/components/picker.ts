import {
  BoxRenderable,
  InputRenderable,
  InputRenderableEvents,
  SelectRenderable,
  SelectRenderableEvents,
  TextRenderable,
  type KeyEvent,
  type RenderContext,
} from "@opentui/core"

import type { RottweilerTheme } from "../theme"

type Rgb = Readonly<{ r: number; g: number; b: number; a: number }>

export function pickerSelectionColors(theme: RottweilerTheme): {
  readonly background: string
  readonly foreground: string
} {
  let background = theme.primary
  if (parseHex(background).a !== 255) {
    background = theme.mode === "light" ? "#555555" : "#BBBBBB"
  }
  const panelContrast = colorContrast(background, theme.panelRaised)
  if (panelContrast < 1.4) {
    const target = theme.mode === "light" ? "#000000" : "#FFFFFF"
    for (const amount of [0.15, 0.25, 0.35, 0.45, 0.55]) {
      const candidate = mixHex(background, target, amount)
      background = candidate
      if (colorContrast(candidate, theme.panelRaised) >= 1.4) break
    }
  }
  const fallbacks = (theme.mode === "light"
    ? [theme.selectedListItemText, "#000000", "#FFFFFF"]
    : [theme.selectedListItemText, "#FFFFFF", "#000000"])
    .filter((candidate) => parseHex(candidate).a === 255)
  const foreground = fallbacks.find((candidate) => colorContrast(candidate, background) >= 4.5)
    ?? fallbacks.reduce((best, candidate) =>
      colorContrast(candidate, background) > colorContrast(best, background) ? candidate : best,
    )
  return { background, foreground }
}

export function colorContrast(left: string, right: string): number {
  const first = relativeLuminance(parseHex(left))
  const second = relativeLuminance(parseHex(right))
  return (Math.max(first, second) + 0.05) / (Math.min(first, second) + 0.05)
}

function parseHex(value: string): Rgb {
  const match = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})/i.exec(value)
  if (match === null) return { r: 0, g: 0, b: 0, a: 0 }
  return {
    r: Number.parseInt(match[1]!, 16),
    g: Number.parseInt(match[2]!, 16),
    b: Number.parseInt(match[3]!, 16),
    a: value.length >= 9 ? Number.parseInt(value.slice(7, 9), 16) : 255,
  }
}

function relativeLuminance(color: Rgb): number {
  const channel = (value: number) => {
    const normalized = value / 255
    return normalized <= 0.04045
      ? normalized / 12.92
      : ((normalized + 0.055) / 1.055) ** 2.4
  }
  return 0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
}

function mixHex(base: string, target: string, amount: number): string {
  const from = parseHex(base)
  const to = parseHex(target)
  const channel = (left: number, right: number) =>
    Math.round(left + (right - left) * amount).toString(16).padStart(2, "0")
  return `#${channel(from.r, to.r)}${channel(from.g, to.g)}${channel(from.b, to.b)}`
}

export interface PickerItem<T> {
  readonly id: string
  readonly label: string
  readonly description: string
  readonly value: T
  readonly searchText?: string
  /** Render this row as status/context, but never focus or activate it. */
  readonly selectable?: boolean
}

export class FuzzyPickerRenderable<T> extends BoxRenderable {
  readonly input: InputRenderable
  readonly status: TextRenderable
  readonly select: SelectRenderable
  #items: readonly PickerItem<T>[] = []
  #filtered: readonly PickerItem<T>[] = []
  #onSelect: ((item: PickerItem<T>) => void) | undefined
  #onQuery: ((query: string) => void) | undefined
  #query = ""
  #anchored = false
  #compact = false
  #desiredHeight = 12
  #secretMode = false
  #textMode = false
  #secretValue = ""
  #onSecretSubmit: ((secret: string) => void) | undefined
  #onTextSubmit: ((value: string) => void) | undefined
  #textMaxBytes = 2048
  #onKey = (key: KeyEvent) => {
    if (!this.visible) return

    if (this.status.visible && !this.#anchored) {
      // A status surface is deliberately not an action list. Keep all input
      // except Escape from leaking into the composer behind the modal.
      if (key.name === "escape") return
      key.preventDefault()
      key.stopPropagation()
      return
    }

    if (this.#textMode && !key.ctrl && !key.meta && !key.option) {
      if (key.name === "return" || key.name === "kpenter") {
        const value = this.input.value.trim()
        if (value.length > 0) {
          const onSubmit = this.#onTextSubmit
          this.#clearInputModes()
          onSubmit?.(value)
        }
      } else if (key.name === "backspace" || key.name === "delete") {
        this.input.value = Array.from(this.input.value).slice(0, -1).join("")
      } else if (isPrintableInput(key.sequence)) {
        const candidate = this.input.value + key.sequence
        if (new TextEncoder().encode(candidate).length <= this.#textMaxBytes) {
          this.input.value = candidate
        }
      } else {
        return
      }
      key.preventDefault()
      key.stopPropagation()
      return
    }

    if (this.#secretMode) {
      const plain = !key.ctrl && !key.meta && !key.option
      if (plain && (key.name === "return" || key.name === "kpenter")) {
        if (this.#secretValue.length > 0) {
          const secret = this.#secretValue
          const onSubmit = this.#onSecretSubmit
          this.#clearInputModes()
          onSubmit?.(secret)
        }
      } else if (plain && (key.name === "backspace" || key.name === "delete")) {
        this.#secretValue = Array.from(this.#secretValue).slice(0, -1).join("")
        this.#renderSecretMask()
      } else if (plain && isPrintableInput(key.sequence)) {
        if (new TextEncoder().encode(this.#secretValue).length + new TextEncoder().encode(key.sequence).length <= 8 * 1024) {
          this.#secretValue += key.sequence
          this.#renderSecretMask()
        }
      } else {
        return
      }
      key.preventDefault()
      key.stopPropagation()
      return
    }

    const plain = !key.ctrl && !key.meta && !key.option && !key.shift
    const controlOnly = key.ctrl && !key.meta && !key.option && !key.shift
    let handled = true
    if ((plain && key.name === "up") || (controlOnly && key.name === "p")) {
      this.moveSelection(-1)
    } else if ((plain && key.name === "down") || (controlOnly && key.name === "n")) {
      this.moveSelection(1)
    } else if (plain && key.name === "pageup") {
      this.moveSelection(-10, false)
    } else if (plain && key.name === "pagedown") {
      this.moveSelection(10, false)
    } else if (plain && key.name === "home") {
      this.moveToBoundary(false)
    } else if (plain && key.name === "end") {
      this.moveToBoundary(true)
    } else if (
      this.select.options.length > 0 &&
      ((plain && (key.name === "return" || key.name === "kpenter")) ||
        (this.#anchored && plain && key.name === "tab"))
    ) {
      this.select.selectCurrent()
    } else {
      // Escape deliberately reaches RottweilerApp so its existing overlay
      // lifecycle restores the correct composer/Vim focus.
      handled = false
    }
    if (handled) {
      key.preventDefault()
      key.stopPropagation()
    }
  }
  #onPaste = (event: { bytes: Uint8Array; preventDefault(): void; stopPropagation(): void }) => {
    if (this.visible && this.status.visible && !this.#anchored) {
      event.preventDefault()
      event.stopPropagation()
      return
    }
    if (!this.visible || (!this.#secretMode && !this.#textMode)) return
    let pasted: string
    try {
      pasted = new TextDecoder("utf-8", { fatal: true }).decode(event.bytes)
    } catch {
      event.preventDefault()
      event.stopPropagation()
      return
    }
    if (!isPrintableInput(pasted)) {
      event.preventDefault()
      event.stopPropagation()
      return
    }
    if (this.#textMode) {
      const candidate = this.input.value + pasted
      if (new TextEncoder().encode(candidate).length <= this.#textMaxBytes) {
        this.input.value = candidate
      }
      event.preventDefault()
      event.stopPropagation()
      return
    }
    const bytes = new TextEncoder().encode(this.#secretValue + pasted)
    if (bytes.length <= 8 * 1024) this.#secretValue += pasted
    this.#renderSecretMask()
    event.preventDefault()
    event.stopPropagation()
  }

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    onQuery?: (query: string) => void,
  ) {
    const selected = pickerSelectionColors(theme)
    super(ctx, {
      id: "fuzzy-picker",
      width: "100%",
      height: 12,
      maxHeight: "80%",
      flexDirection: "column",
      border: true,
      borderStyle: "rounded",
      borderColor: theme.border,
      focusedBorderColor: theme.focus,
      backgroundColor: theme.panelRaised,
      padding: 1,
      gap: 1,
      visible: false,
      zIndex: 20,
    })
    this.#onQuery = onQuery
    this.input = new InputRenderable(ctx, {
      id: "picker-query",
      width: "100%",
      placeholder: "type to filter…",
      backgroundColor: theme.panel,
      textColor: theme.foreground,
      focusedBackgroundColor: theme.selection,
      focusedTextColor: theme.foreground,
    })
    this.select = new SelectRenderable(ctx, {
      id: "picker-results",
      width: "100%",
      flexGrow: 1,
      options: [],
      backgroundColor: theme.panelRaised,
      textColor: theme.foreground,
      selectedBackgroundColor: selected.background,
      selectedTextColor: selected.foreground,
      descriptionColor: theme.muted,
      selectedDescriptionColor: selected.foreground,
      showScrollIndicator: true,
      wrapSelection: true,
      fastScrollStep: 10,
    })
    this.status = new TextRenderable(ctx, {
      id: "picker-status",
      width: "100%",
      flexGrow: 1,
      content: "",
      fg: theme.muted,
      visible: false,
    })
    this.add(this.input)
    this.add(this.status)
    this.add(this.select)
    this.input.on(InputRenderableEvents.INPUT, (query: string) => {
      this.#filter(query, false)
      this.#onQuery?.(query)
    })
    this.input.on(InputRenderableEvents.ENTER, () => this.select.selectCurrent())
    this.select.on(SelectRenderableEvents.ITEM_SELECTED, (index: number) => {
      const item = this.#filtered[index]
      if (item !== undefined && item.selectable !== false) {
        this.#onSelect?.(item)
      }
    })
    this.select.onMouseDown = (event) => {
      if (event.button !== 0) return
      const index = this.#mouseIndex(event.y)
      if (index === null) return
      if (this.#filtered[index]?.selectable === false) {
        event.preventDefault()
        event.stopPropagation()
        return
      }
      this.#setSelectionAtEdge(index)
      event.preventDefault()
      event.stopPropagation()
    }
    this.select.onMouseUp = (event) => {
      if (event.button !== 0) return
      const index = this.#mouseIndex(event.y)
      if (index === null) return
      if (this.#filtered[index]?.selectable === false) {
        event.preventDefault()
        event.stopPropagation()
        return
      }
      this.#setSelectionAtEdge(index)
      this.select.selectCurrent()
      event.preventDefault()
      event.stopPropagation()
    }
    this.select.onMouseScroll = (event) => {
      const direction = event.scroll?.direction
      if (direction !== "up" && direction !== "down") return
      this.#scrollViewport(direction === "up" ? -1 : 1)
      event.preventDefault()
      event.stopPropagation()
    }
    ctx.keyInput.on("keypress", this.#onKey)
    ctx.keyInput.on("paste", this.#onPaste)
  }

  open(
    title: string,
    items: readonly PickerItem<T>[],
    onSelect: (item: PickerItem<T>) => void,
    compact = false,
  ): void {
    this.#clearInputModes()
    this.status.visible = false
    this.input.visible = true
    this.select.visible = true
    this.#configurePresentation(false, items.length, compact)
    this.title = ` ${title} `
    this.#items = items
    this.#onSelect = onSelect
    this.input.value = ""
    this.#query = ""
    this.#filter("", false)
    this.visible = true
    this.input.focus()
  }

  openSecret(title: string, onSubmit: (secret: string) => void): void {
    this.#clearInputModes()
    this.#secretMode = true
    this.#onSecretSubmit = onSubmit
    this.#configurePresentation(false, 0)
    this.title = ` ${title} `
    this.#items = []
    this.#filtered = []
    this.select.options = []
    this.select.visible = false
    this.status.visible = false
    this.input.visible = true
    this.input.placeholder = "API key (hidden)"
    this.input.value = ""
    this.visible = true
    this.height = 5
    this.input.focus()
  }

  openTextPrompt(title: string, placeholder: string, onSubmit: (value: string) => void, maxBytes = 2048): void {
    this.#clearInputModes()
    this.#textMode = true
    this.#onTextSubmit = onSubmit
    this.#textMaxBytes = Math.max(1, Math.min(maxBytes, 8192))
    this.#configurePresentation(false, 0)
    this.title = ` ${title} `
    this.#items = []
    this.#filtered = []
    this.select.options = []
    this.select.visible = false
    this.status.visible = false
    this.input.visible = true
    this.input.placeholder = placeholder
    this.input.value = ""
    this.visible = true
    this.height = 5
    this.input.focus()
  }

  /** Present transient or empty state without masquerading as a selectable row. */
  showStatus(title: string, message: string, description = "", anchored = false): void {
    this.#clearInputModes()
    this.#anchored = anchored
    this.title = ` ${title} `
    this.#items = []
    this.#filtered = []
    this.#onSelect = undefined
    this.input.blur()
    this.select.options = []
    this.input.visible = false
    this.select.visible = false
    this.status.content = description.length === 0 ? message : `${message}\n${description}`
    this.status.visible = true
    this.visible = true
    this.#desiredHeight = description.length === 0 ? 5 : 6
    this.height = this.#desiredHeight
  }

  showLoading(title: string, message: string, anchored = false): void {
    this.showStatus(title, `◌ ${message}`, "This panel will update automatically.", anchored)
  }

  /** Replace remote results without clearing the query or moving focus. */
  refresh(
    title: string,
    items: readonly PickerItem<T>[],
    onSelect: (item: PickerItem<T>) => void,
    compact = false,
  ): void {
    if (!this.visible) {
      this.open(title, items, onSelect, compact)
      return
    }
    this.#configurePresentation(false, items.length, compact)
    this.status.visible = false
    this.input.visible = true
    this.select.visible = true
    this.title = ` ${title} `
    this.#items = items
    this.#onSelect = onSelect
    this.#query = this.input.value
    this.#filter(this.input.value, true)
  }

  /** Composer-anchored autocomplete keeps editing focus in the textarea. */
  openAnchored(
    title: string,
    items: readonly PickerItem<T>[],
    query: string,
    onSelect: (item: PickerItem<T>) => void,
  ): void {
    this.#configurePresentation(true, items.length)
    this.status.visible = false
    this.select.visible = true
    this.title = ` ${title} `
    this.#items = items
    this.#onSelect = onSelect
    this.#query = query
    this.#filter(query, false)
    this.visible = true
  }

  /** Refresh composer autocomplete without stealing focus. A new query selects its best match. */
  refreshAnchored(
    title: string,
    items: readonly PickerItem<T>[],
    query: string,
    onSelect: (item: PickerItem<T>) => void,
  ): void {
    if (!this.visible || !this.#anchored) {
      this.openAnchored(title, items, query, onSelect)
      return
    }
    this.#configurePresentation(true, items.length)
    this.status.visible = false
    this.select.visible = true
    this.title = ` ${title} `
    this.#items = items
    this.#onSelect = onSelect
    const queryChanged = query !== this.#query
    this.#query = query
    this.#filter(query, !queryChanged)
  }

  get anchored(): boolean {
    return this.#anchored
  }

  constrainAnchoredHeight(availableRows: number): number {
    const height = Math.max(1, Math.min(this.#desiredHeight, Math.floor(availableRows)))
    if (this.#anchored) this.height = height
    return height
  }

  constrainModalHeight(availableRows: number): number {
    const height = Math.max(1, Math.min(this.#desiredHeight, Math.floor(availableRows)))
    if (!this.#anchored) this.height = height
    return height
  }

  /** OpenCode-style keyboard navigation keeps the active result centered. */
  moveSelection(delta: number, wrap = true): void {
    const selectable = this.#selectableIndices()
    if (selectable.length === 0) return
    const current = this.select.getSelectedIndex()
    const currentPosition = selectable.indexOf(current)
    const origin = currentPosition >= 0 ? currentPosition : 0
    let target = origin + delta
    if (wrap) target = ((target % selectable.length) + selectable.length) % selectable.length
    else target = Math.min(Math.max(target, 0), selectable.length - 1)
    this.#setKeyboardSelection(selectable[target]!)
  }

  moveToBoundary(end: boolean): void {
    const selectable = this.#selectableIndices()
    if (selectable.length === 0) return
    this.#setKeyboardSelection(
      end ? selectable.at(-1)! : selectable[0]!,
      end ? "end" : "start",
    )
  }

  close(): void {
    this.visible = false
    this.input.blur()
    this.input.visible = true
    this.select.visible = true
    this.status.visible = false
    this.status.content = ""
    this.input.placeholder = "type to filter…"
    this.#anchored = false
    this.#onSelect = undefined
    this.#clearInputModes()
    this.#items = []
    this.#filtered = []
    this.#query = ""
    this.select.options = []
    this.input.value = ""
  }

  override destroy(): void {
    this.ctx.keyInput.off("keypress", this.#onKey)
    this.ctx.keyInput.off("paste", this.#onPaste)
    super.destroy()
  }

  #renderSecretMask(): void {
    const length = Array.from(this.#secretValue).length
    this.input.value = `${"•".repeat(Math.min(length, 64))}${length > 64 ? "…" : ""}`
  }

  #clearInputModes(): void {
    this.#secretValue = ""
    this.#secretMode = false
    this.#onSecretSubmit = undefined
    this.#textMode = false
    this.#onTextSubmit = undefined
    this.#textMaxBytes = 2048
    this.input.value = ""
  }

  #configurePresentation(anchored: boolean, itemCount: number, compact = false): void {
    this.#anchored = anchored
    this.#compact = !anchored && compact
    this.input.visible = !anchored
    this.select.showDescription = !this.#compact
    this.gap = anchored ? 0 : 1
    const compactLimit = Math.max(5, Math.floor(this.ctx.height / 2))
    this.#desiredHeight = anchored
      ? Math.min(12, Math.max(3, itemCount + 2))
      : this.#compact
        ? Math.min(14, compactLimit, Math.max(5, itemCount + 3))
        : 12
    this.height = this.#desiredHeight
  }

  #filter(query: string, preserveSelection = false): void {
    const selectedId = preserveSelection ? this.select.getSelectedOption()?.value : undefined
    const selectedIndex = preserveSelection ? this.select.getSelectedIndex() : 0
    const scrollOffset = preserveSelection ? this.#scrollOffset() : 0
    const ranked = this.#items
      .map((item, index) => ({
        item,
        index,
        score: pickerItemScore(query, item),
      }))
      .filter((entry) => entry.score !== null)
      .sort((left, right) => (right.score ?? 0) - (left.score ?? 0) || left.index - right.index)
    this.#filtered = ranked.map((entry) => entry.item)
    this.select.options = this.#filtered.map((item) => ({
      name: item.label,
      description: item.description,
      value: item.id,
    }))
    if (this.#filtered.length === 0) return
    const retainedIndex = this.#filtered.findIndex((item) => item.id === selectedId)
    const candidateIndex =
      retainedIndex >= 0
        ? retainedIndex
        : Math.min(Math.max(selectedIndex, 0), this.#filtered.length - 1)
    const nextIndex = this.#nearestSelectableIndex(candidateIndex)
    if (nextIndex === null) return
    this.select.setSelectedIndex(nextIndex)
    if (preserveSelection) {
      this.#setScrollOffset(
        Math.min(scrollOffset, Math.max(0, this.#filtered.length - this.#visibleItemCount())),
      )
    }
  }

  #setSelectionAtEdge(target: number, boundary?: "start" | "end"): void {
    if (this.#filtered[target]?.selectable === false) return
    const previousOffset = this.#scrollOffset()
    const visible = this.#visibleItemCount()
    this.select.setSelectedIndex(target)
    const nextOffset =
      boundary === "start"
        ? 0
        : boundary === "end"
          ? Math.max(0, this.select.options.length - visible)
          : target < previousOffset
            ? target
            : target >= previousOffset + visible
              ? target - visible + 1
              : previousOffset
    this.#setScrollOffset(nextOffset)
  }

  #setKeyboardSelection(target: number, boundary?: "start" | "end"): void {
    if (this.#filtered[target]?.selectable === false) return
    const visible = this.#visibleItemCount()
    this.select.setSelectedIndex(target)
    const maximum = Math.max(0, this.select.options.length - visible)
    const nextOffset =
      boundary === "start"
        ? 0
        : boundary === "end"
          ? maximum
          : Math.min(maximum, Math.max(0, target - Math.floor(visible / 2)))
    this.#setScrollOffset(nextOffset)
  }

  #scrollViewport(delta: number): void {
    const maximum = Math.max(0, this.select.options.length - this.#visibleItemCount())
    this.#setScrollOffset(Math.min(maximum, Math.max(0, this.#scrollOffset() + delta)))
  }

  #visibleItemCount(): number {
    return Math.max(1, Math.floor(this.select.height / (this.#compact ? 1 : 2)))
  }

  #scrollOffset(): number {
    return (this.select as unknown as { scrollOffset: number }).scrollOffset
  }

  #setScrollOffset(value: number): void {
    ;(this.select as unknown as { scrollOffset: number }).scrollOffset = Math.max(0, value)
    this.select.requestRender()
  }

  #mouseIndex(mouseY: number): number | null {
    const localRow = Math.floor(mouseY - this.select.y)
    if (localRow < 0 || localRow >= this.select.height) return null
    const index = this.#scrollOffset() + Math.floor(localRow / (this.#compact ? 1 : 2))
    return index >= 0 && index < this.select.options.length ? index : null
  }

  #selectableIndices(): number[] {
    const selectable: number[] = []
    for (let index = 0; index < this.#filtered.length; index += 1) {
      if (this.#filtered[index]?.selectable !== false) selectable.push(index)
    }
    return selectable
  }

  #nearestSelectableIndex(origin: number): number | null {
    if (this.#filtered[origin]?.selectable !== false) return origin
    for (let distance = 1; distance < this.#filtered.length; distance += 1) {
      const after = origin + distance
      if (after < this.#filtered.length && this.#filtered[after]?.selectable !== false) return after
      const before = origin - distance
      if (before >= 0 && this.#filtered[before]?.selectable !== false) return before
    }
    return null
  }
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
  const needle = query.trim().toLocaleLowerCase()
  const haystack = candidate.toLocaleLowerCase()
  if (needle.length === 0) {
    return 0
  }
  let cursor = 0
  let score = 0
  let streak = 0
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
    cursor += 1
  }
  return cursor === needle.length ? score - (haystack.length - needle.length) * 0.05 : null
}
