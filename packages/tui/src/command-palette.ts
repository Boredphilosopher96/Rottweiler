import { fuzzyMatch, type FuzzyMatch } from "./components/picker"

export type CommandPaletteSource = "builtin" | "extension"

export interface CommandPaletteEntry<Action> {
  readonly id: string
  readonly title: string
  readonly description: string
  readonly section: string
  readonly source: CommandPaletteSource
  readonly action: Action
}

export type CommandPaletteCatalog =
  | { readonly kind: "loading" }
  | { readonly kind: "ready"; readonly truncated: boolean }
  | { readonly kind: "error"; readonly message: string; readonly retryable: boolean }

export type CommandPaletteNotice =
  | { readonly kind: "loading"; readonly message: string }
  | { readonly kind: "error"; readonly message: string; readonly retryable: boolean }
  | { readonly kind: "truncated"; readonly message: string }
  | null

export interface CommandPaletteSectionRow {
  readonly kind: "section"
  readonly id: string
  readonly label: string
}

export interface CommandPaletteItemRow<Action> extends CommandPaletteEntry<Action> {
  readonly kind: "item"
  readonly score: number
  readonly titleMatches: readonly (readonly [start: number, end: number])[]
}

export type CommandPaletteRow<Action> =
  | CommandPaletteSectionRow
  | CommandPaletteItemRow<Action>

export type CommandPaletteDetail =
  | {
      readonly kind: "command"
      readonly id: string
      readonly title: string
      readonly description: string
      readonly section: string
      readonly source: CommandPaletteSource
    }
  | { readonly kind: "empty"; readonly message: string }

export interface CommandPaletteCounts {
  readonly visible: number
  readonly total: number
  readonly builtIn: number
  readonly extension: number
}

export interface CommandPaletteModel<Action> {
  readonly rows: readonly CommandPaletteRow<Action>[]
  readonly selectedId: string | null
  readonly detail: CommandPaletteDetail
  readonly counts: CommandPaletteCounts
  readonly status: string
  readonly notice: CommandPaletteNotice
}

interface CommandPaletteModelOptions<Action> {
  readonly entries: readonly CommandPaletteEntry<Action>[]
  readonly sections: readonly string[]
  readonly query: string
  readonly selectedId: string | null
  readonly catalog: CommandPaletteCatalog
}

interface RankedEntry<Action> {
  readonly entry: CommandPaletteEntry<Action>
  readonly index: number
  readonly score: number
  readonly titleMatch: FuzzyMatch | null
}

export { fuzzyMatch }

export function retainCommandPaletteSelection<Action>(
  entries: readonly CommandPaletteEntry<Action>[],
  selectedId: string | null,
): string | null {
  if (selectedId !== null && entries.some((entry) => entry.id === selectedId)) {
    return selectedId
  }
  return entries[0]?.id ?? null
}

export function createCommandPaletteModel<Action>(
  options: CommandPaletteModelOptions<Action>,
): CommandPaletteModel<Action> {
  const query = options.query.trim()
  const ranked = options.entries
    .map((entry, index): RankedEntry<Action> | null => {
      const titleMatch = fuzzyMatch(query, entry.title)
      const searchMatch = fuzzyMatch(
        query,
        `${entry.section} ${entry.title} ${entry.description}`,
      )
      const descriptionMatch = fuzzyMatch(query, entry.description)
      const scores: number[] = []
      if (titleMatch !== null) {
        const label = entry.title.toLocaleLowerCase()
        const needle = query.toLocaleLowerCase()
        const exact = label === needle || label === `/${needle}`
        const prefix = label.startsWith(needle) || label.startsWith(`/${needle}`)
        scores.push(titleMatch.score + (exact ? 1_000 : prefix ? 500 : 200))
      }
      if (searchMatch !== null) scores.push(searchMatch.score + 100)
      if (descriptionMatch !== null) scores.push(descriptionMatch.score)
      if (query.length > 0 && scores.length === 0) return null
      return {
        entry,
        index,
        score: scores.length === 0 ? 0 : Math.max(...scores),
        titleMatch,
      }
    })
    .filter((entry): entry is RankedEntry<Action> => entry !== null)
    .sort((left, right) => right.score - left.score || left.index - right.index)

  const visibleEntries = ranked.map(({ entry }) => entry)
  const selectedId = retainCommandPaletteSelection(visibleEntries, options.selectedId)
  const rows: CommandPaletteRow<Action>[] = []
  if (query.length === 0) {
    for (const section of options.sections) {
      const sectionEntries = ranked.filter(({ entry }) => entry.section === section)
      if (sectionEntries.length === 0) continue
      rows.push({ kind: "section", id: sectionId(section), label: section })
      rows.push(...sectionEntries.map(toItemRow))
    }
  } else {
    rows.push(...ranked.map(toItemRow))
  }

  const selected = selectedId === null
    ? undefined
    : options.entries.find((entry) => entry.id === selectedId)
  const total = options.entries.length
  const builtIn = options.entries.filter((entry) => entry.source === "builtin").length
  const extension = total - builtIn
  const counts = { visible: visibleEntries.length, total, builtIn, extension }

  return {
    rows,
    selectedId,
    detail: selected === undefined
      ? {
          kind: "empty",
          message: total === 0 ? "No commands available" : "No matching commands",
        }
      : {
          kind: "command",
          id: selected.id,
          title: selected.title,
          description: selected.description,
          section: selected.section,
          source: selected.source,
        },
    counts,
    status: query.length === 0
      ? `${total} ${plural(total, "command")} · ${builtIn} built-in · ${extension} ${plural(extension, "extension")}`
      : `${visibleEntries.length} of ${total} ${plural(total, "command")} · ${builtIn} built-in · ${extension} ${plural(extension, "extension")}`,
    notice: catalogNotice(options.catalog),
  }
}

function toItemRow<Action>(entry: RankedEntry<Action>): CommandPaletteItemRow<Action> {
  return {
    kind: "item",
    ...entry.entry,
    score: entry.score,
    titleMatches: matchSpans(entry.entry.title, entry.titleMatch),
  }
}

function matchSpans(
  candidate: string,
  match: FuzzyMatch | null,
): readonly (readonly [number, number])[] {
  if (match === null) return []
  const positions = match.positions.filter((position) => !/\s/u.test(candidate[position] ?? ""))
  const spans: Array<readonly [number, number]> = []
  for (const position of positions) {
    const previous = spans.at(-1)
    if (previous !== undefined && previous[1] === position) {
      spans[spans.length - 1] = [previous[0], position + 1]
    } else {
      spans.push([position, position + 1])
    }
  }
  return spans
}

function catalogNotice(catalog: CommandPaletteCatalog): CommandPaletteNotice {
  switch (catalog.kind) {
    case "loading":
      return { kind: "loading", message: "Loading extension commands…" }
    case "error":
      return { kind: "error", message: catalog.message, retryable: catalog.retryable }
    case "ready":
      return catalog.truncated
        ? { kind: "truncated", message: "Extension results are truncated" }
        : null
  }
}

function sectionId(section: string): string {
  return `section.${section.toLocaleLowerCase().replace(/[^a-z0-9]+/g, "-")}`
}

function plural(count: number, singular: string): string {
  return count === 1 ? singular : `${singular}s`
}
