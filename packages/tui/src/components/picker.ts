import {
  BoxRenderable,
  InputRenderable,
  InputRenderableEvents,
  SelectRenderable,
  SelectRenderableEvents,
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

  constructor(
    ctx: RenderContext,
    theme: RottweilerTheme,
    onQuery?: (query: string) => void,
  ) {
    super(ctx, {
      id: "fuzzy-picker",
      width: "100%",
      height: 12,
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
    })
    this.add(this.input)
    this.add(this.select)
    this.input.on(InputRenderableEvents.INPUT, (query: string) => {
      this.#filter(query)
      this.#onQuery?.(query)
    })
    this.input.on(InputRenderableEvents.ENTER, () => this.select.selectCurrent())
    this.select.on(SelectRenderableEvents.ITEM_SELECTED, (index: number) => {
      const item = this.#filtered[index]
      if (item !== undefined) {
        this.#onSelect?.(item)
      }
    })
  }

  open(
    title: string,
    items: readonly PickerItem<T>[],
    onSelect: (item: PickerItem<T>) => void,
  ): void {
    this.title = ` ${title} `
    this.#items = items
    this.#onSelect = onSelect
    this.input.value = ""
    this.#filter("")
    this.visible = true
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
    this.title = ` ${title} `
    this.#items = items
    this.#onSelect = onSelect
    this.#filter(this.input.value)
  }

  close(): void {
    this.visible = false
    this.input.blur()
    this.#onSelect = undefined
  }

  #filter(query: string): void {
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
    this.select.setSelectedIndex(0)
  }
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
