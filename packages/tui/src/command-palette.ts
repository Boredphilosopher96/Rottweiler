import { COMMAND_SECTIONS } from "../../../protocol/types"
import { fuzzyMatch, type FuzzyMatch } from "./components/picker"

export const EXTENSIONS_SECTION = "Extensions"
export const RECENT_SECTION = "Recent"
export const MAX_RECENT_COMMANDS = 3

export type CommandSectionName = (typeof COMMAND_SECTIONS)[number] | typeof EXTENSIONS_SECTION

/** Sections in display order; extension commands always follow built-ins. */
export const COMMAND_SECTION_ORDER: readonly CommandSectionName[] = [...COMMAND_SECTIONS, EXTENSIONS_SECTION]

/** One discoverable command shared by slash completion and the Ctrl+P palette. */
export interface CommandEntry<Action> {
  readonly id: string
  /** Slash name without the leading `/`. */
  readonly name: string
  readonly aliases: readonly string[]
  readonly title: string
  readonly section: CommandSectionName
  readonly description: string
  readonly argumentHint: string
  readonly keycap: string | null
  /** Extension provenance such as `skill` or `mcp · github`; null for built-ins. */
  readonly sourceLabel: string | null
  readonly unavailableReason: string | null
  readonly action: Action
}

export type CommandPaletteCatalog =
  | { readonly kind: "loading" }
  | { readonly kind: "ready"; readonly truncated: boolean }
  | { readonly kind: "error"; readonly message: string }

export type CommandPaletteNotice =
  | { readonly kind: "loading"; readonly message: string }
  | { readonly kind: "error"; readonly message: string }
  | { readonly kind: "truncated"; readonly message: string }
  | null

export interface CommandSectionRow {
  readonly kind: "section"
  readonly id: string
  readonly label: string
}

export interface CommandItemRow<Action> {
  readonly kind: "item"
  readonly id: string
  readonly entry: CommandEntry<Action>
  readonly titleMatches: readonly (readonly [start: number, end: number])[]
}

export type CommandRow<Action> = CommandSectionRow | CommandItemRow<Action>

export interface CommandList<Action> {
  readonly rows: readonly CommandRow<Action>[]
  readonly selectedId: string | null
  readonly visible: number
}

export interface CommandPaletteModel<Action> extends CommandList<Action> {
  readonly total: number
  readonly status: string
  readonly notice: CommandPaletteNotice
}

export { fuzzyMatch }

/** Normalizes palette and slash input to the command-name query it ranks. */
export function commandQuery(query: string): string {
  return query.trim().replace(/^\//, "").toLocaleLowerCase()
}

interface Ranked<Action> {
  readonly entry: CommandEntry<Action>
  readonly index: number
  readonly tier: number
  readonly score: number
  readonly titleMatch: FuzzyMatch | null
}

/**
 * Ranks by exact name or alias, then name prefix, title word prefix, fuzzy
 * title, and description. Ties prefer recent use, then section order.
 */
export function rankCommands<Action>(
  entries: readonly CommandEntry<Action>[],
  query: string,
  recent: readonly string[],
): readonly Ranked<Action>[] {
  const needle = commandQuery(query)
  const ranked: Ranked<Action>[] = []
  entries.forEach((entry, index) => {
    const titleMatch = needle.length === 0 ? null : fuzzyMatch(needle, entry.title)
    const match = rankEntry(entry, needle, titleMatch)
    if (match !== null) ranked.push({ entry, index, titleMatch, ...match })
  })
  return ranked.sort((left, right) =>
    right.tier - left.tier
    || recency(left.entry.id, recent) - recency(right.entry.id, recent)
    || right.score - left.score
    || sectionIndex(left.entry.section) - sectionIndex(right.entry.section)
    || left.index - right.index)
}

function rankEntry<Action>(
  entry: CommandEntry<Action>,
  needle: string,
  titleMatch: FuzzyMatch | null,
): { readonly tier: number; readonly score: number } | null {
  if (needle.length === 0) return { tier: 0, score: 0 }
  const names = [entry.name, ...entry.aliases]
  if (names.includes(needle)) return { tier: 5, score: 0 }
  if (names.some((name) => name.startsWith(needle))) return { tier: 4, score: 0 }
  const title = entry.title.toLocaleLowerCase()
  const words = title.split(/[\s&·/_.-]+/u).filter((word) => word.length > 0)
  const parts = needle.split(/\s+/u)
  if (title.startsWith(needle) || parts.every((part) => words.some((word) => word.startsWith(part)))) {
    return { tier: 3, score: 0 }
  }
  // Single characters only match names and word starts; fuzzy matching one
  // letter anywhere would list nearly every command.
  if (needle.length < 2) return null
  if (titleMatch !== null) return { tier: 2, score: titleMatch.score }
  const description = entry.description.toLocaleLowerCase().split(/[\s,·/()-]+/u)
  return parts.every((part) => description.some((word) => word.startsWith(part))) ? { tier: 1, score: 0 } : null
}

/**
 * Builds grouped rows for an empty query (Recent first, unavailable entries last
 * in each section) or a flat ranked list. Unavailable entries are never the
 * initial selection.
 */
export function createCommandList<Action>(
  entries: readonly CommandEntry<Action>[],
  query: string,
  recent: readonly string[],
  selectedId: string | null = null,
): CommandList<Action> {
  const rows: CommandRow<Action>[] = []
  if (commandQuery(query).length === 0) {
    const recentEntries = recent
      .map((id) => entries.find((entry) => entry.id === id))
      .filter((entry): entry is CommandEntry<Action> =>
        entry !== undefined && entry.unavailableReason === null)
      .slice(0, MAX_RECENT_COMMANDS)
    const recentIds = new Set(recentEntries.map((entry) => entry.id))
    if (recentEntries.length > 0) {
      rows.push(sectionRow(RECENT_SECTION))
      rows.push(...recentEntries.map((entry) => itemRow(entry, null)))
    }
    for (const section of COMMAND_SECTION_ORDER) {
      const members = entries.filter((entry) => entry.section === section && !recentIds.has(entry.id))
      if (members.length === 0) continue
      rows.push(sectionRow(section))
      rows.push(...members.filter((entry) => entry.unavailableReason === null).map((entry) => itemRow(entry, null)))
      rows.push(...members.filter((entry) => entry.unavailableReason !== null).map((entry) => itemRow(entry, null)))
    }
  } else {
    rows.push(...rankCommands(entries, query, recent).map((ranked) => itemRow(ranked.entry, ranked.titleMatch)))
  }
  const items = rows.filter((row): row is CommandItemRow<Action> => row.kind === "item")
  const retained = items.find((row) => row.id === selectedId && row.entry.unavailableReason === null)
  return {
    rows,
    selectedId: retained?.id ?? items.find((row) => row.entry.unavailableReason === null)?.id ?? null,
    visible: items.length,
  }
}

export function createCommandPaletteModel<Action>(options: {
  readonly entries: readonly CommandEntry<Action>[]
  readonly query: string
  readonly recent: readonly string[]
  readonly selectedId: string | null
  readonly catalog: CommandPaletteCatalog
}): CommandPaletteModel<Action> {
  const list = createCommandList(options.entries, options.query, options.recent, options.selectedId)
  const total = options.entries.length
  return {
    ...list,
    total,
    status: commandQuery(options.query).length === 0
      ? `${total} ${plural(total, "command")}`
      : `${list.visible} of ${total} ${plural(total, "command")}`,
    notice: catalogNotice(options.catalog),
  }
}

function itemRow<Action>(entry: CommandEntry<Action>, match: FuzzyMatch | null): CommandItemRow<Action> {
  return { kind: "item", id: entry.id, entry, titleMatches: matchSpans(entry.title, match) }
}

function sectionRow(label: string): CommandSectionRow {
  return { kind: "section", id: `section.${label.toLocaleLowerCase().replace(/[^a-z0-9]+/g, "-")}`, label }
}

function recency(id: string, recent: readonly string[]): number {
  const index = recent.indexOf(id)
  return index < 0 ? Number.MAX_SAFE_INTEGER : index
}

function sectionIndex(section: CommandSectionName): number {
  return COMMAND_SECTION_ORDER.indexOf(section)
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
      return { kind: "error", message: catalog.message }
    case "ready":
      return catalog.truncated
        ? { kind: "truncated", message: "Extension results are truncated" }
        : null
  }
}

/** Records one use, most recent first, without duplicates. */
export function rememberCommand(recent: readonly string[], id: string, limit = 8): readonly string[] {
  return [id, ...recent.filter((candidate) => candidate !== id)].slice(0, limit)
}

function plural(count: number, singular: string): string {
  return count === 1 ? singular : `${singular}s`
}
