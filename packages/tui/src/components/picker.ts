import {
  BoxRenderable,
  InputRenderable,
  InputRenderableEvents,
  SelectRenderable,
  SelectRenderableEvents,
  type KeyEvent,
  type RenderContext,
} from "@opentui/core"

import type { RottweilerTheme } from "../theme"

export interface PickerItem<T> {
  readonly id: string
  readonly label: string
  readonly description: string
  readonly value: T
  readonly searchText?: string
}

export class FuzzyPickerRenderable<T> extends BoxRenderable {
  readonly input: InputRenderable
  readonly select: SelectRenderable
  #items: readonly PickerItem<T>[] = []
  #filtered: readonly PickerItem<T>[] = []
  #onSelect: ((item: PickerItem<T>) => void) | undefined
  #onQuery: ((query: string) => void) | undefined
  #anchored = false
  #desiredHeight = 12
  #secretMode = false
  #textMode = false
  #secretValue = ""
  #onSecretSubmit: ((secret: string) => void) | undefined
  #onTextSubmit: ((value: string) => void) | undefined
  #textMaxBytes = 2048
  #onKey = (key: KeyEvent) => {
    if (!this.visible) return


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
      selectedBackgroundColor: theme.selection,
      selectedTextColor: theme.accentStrong,
      descriptionColor: theme.muted,
      selectedDescriptionColor: theme.foreground,
      showScrollIndicator: true,
      wrapSelection: true,
      fastScrollStep: 10,
    })
    this.add(this.input)
    this.add(this.select)
    this.input.on(InputRenderableEvents.INPUT, (query: string) => {
      this.#filter(query, false)
      this.#onQuery?.(query)
    })
    this.input.on(InputRenderableEvents.ENTER, () => this.select.selectCurrent())
    this.select.on(SelectRenderableEvents.ITEM_SELECTED, (index: number) => {
      const item = this.#filtered[index]
      if (item !== undefined) {
        this.#onSelect?.(item)
      }
    })
    this.select.onMouseDown = (event) => {
      if (event.button !== 0) return
      const index = this.#mouseIndex(event.y)
      if (index === null) return
      this.#setSelectionAtEdge(index)
      event.preventDefault()
      event.stopPropagation()
    }
    this.select.onMouseUp = (event) => {
      if (event.button !== 0) return
      const index = this.#mouseIndex(event.y)
      if (index === null) return
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
  ): void {
    this.#clearInputModes()
    this.#configurePresentation(false, items.length)
    this.title = ` ${title} `
    this.#items = items
    this.#onSelect = onSelect
    this.input.value = ""
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
    this.input.placeholder = placeholder
    this.input.value = ""
    this.visible = true
    this.height = 5
    this.input.focus()
  }

  /** Replace remote results without clearing the query or moving focus. */
  refresh(
    title: string,
    items: readonly PickerItem<T>[],
    onSelect: (item: PickerItem<T>) => void,
  ): void {
    if (!this.visible) {
      this.open(title, items, onSelect)
      return
    }
    this.#configurePresentation(false, items.length)
    this.title = ` ${title} `
    this.#items = items
    this.#onSelect = onSelect
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
    this.title = ` ${title} `
    this.#items = items
    this.#onSelect = onSelect
    this.#filter(query, false)
    this.visible = true
  }

  /** Refresh composer autocomplete without stealing focus or jumping selection. */
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
    this.title = ` ${title} `
    this.#items = items
    this.#onSelect = onSelect
    this.#filter(query, true)
  }

  get anchored(): boolean {
    return this.#anchored
  }

  constrainAnchoredHeight(availableRows: number): void {
    if (!this.#anchored) return
    this.height = Math.max(1, Math.min(this.#desiredHeight, Math.floor(availableRows)))
  }

  /** OpenCode-style keyboard navigation keeps the active result centered. */
  moveSelection(delta: number, wrap = true): void {
    const count = this.select.options.length
    if (count === 0) return
    const current = this.select.getSelectedIndex()
    let target = current + delta
    if (wrap && delta === -1 && current === 0) target = count - 1
    else if (wrap && delta === 1 && current === count - 1) target = 0
    else target = Math.min(Math.max(target, 0), count - 1)
    this.#setKeyboardSelection(target)
  }

  moveToBoundary(end: boolean): void {
    const count = this.select.options.length
    if (count === 0) return
    this.#setKeyboardSelection(end ? count - 1 : 0, end ? "end" : "start")
  }

  close(): void {
    this.visible = false
    this.input.blur()
    this.input.visible = true
    this.select.visible = true
    this.input.placeholder = "type to filter…"
    this.#anchored = false
    this.#onSelect = undefined
    this.#clearInputModes()
    this.#items = []
    this.#filtered = []
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

  #configurePresentation(anchored: boolean, itemCount: number): void {
    this.#anchored = anchored
    this.input.visible = !anchored
    this.gap = anchored ? 0 : 1
    this.#desiredHeight = anchored ? Math.min(12, Math.max(3, itemCount + 2)) : 12
    this.height = this.#desiredHeight
  }

  #filter(query: string, preserveSelection = false): void {
    const selectedId = preserveSelection ? this.select.getSelectedOption()?.value : undefined
    const selectedIndex = preserveSelection ? this.select.getSelectedIndex() : 0
    const scrollOffset = preserveSelection ? this.#scrollOffset() : 0
    const ranked = this.#items
      .map((item) => ({
        item,
        score: fuzzyScore(query, `${item.label} ${item.searchText ?? ""} ${item.description}`),
      }))
      .filter((entry) => entry.score !== null)
      .sort((left, right) => (right.score ?? 0) - (left.score ?? 0))
    this.#filtered = ranked.map((entry) => entry.item)
    this.select.options = this.#filtered.map((item) => ({
      name: item.label,
      description: item.description,
      value: item.id,
    }))
    if (this.#filtered.length === 0) return
    const retainedIndex = this.#filtered.findIndex((item) => item.id === selectedId)
    const nextIndex =
      retainedIndex >= 0
        ? retainedIndex
        : Math.min(Math.max(selectedIndex, 0), this.#filtered.length - 1)
    this.select.setSelectedIndex(nextIndex)
    if (preserveSelection) {
      this.#setScrollOffset(
        Math.min(scrollOffset, Math.max(0, this.#filtered.length - this.#visibleItemCount())),
      )
    }
  }

  #setSelectionAtEdge(target: number, boundary?: "start" | "end"): void {
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
    // Picker rows render a name plus description.
    return Math.max(1, Math.floor(this.select.height / 2))
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
    const index = this.#scrollOffset() + Math.floor(localRow / 2)
    return index >= 0 && index < this.select.options.length ? index : null
  }
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
