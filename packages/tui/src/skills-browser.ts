import { fuzzyMatch } from "./components/picker"
import type {
  ListDetailItemRow,
  ListDetailPresentation,
  ListDetailRow,
} from "./components/list-detail"
import type {
  ExtensionArtifactKind,
  ExtensionArtifactStatus,
  ExtensionInventoryEntry,
} from "../../../protocol/types"

/** What Enter does on one inventory row. */
export type SkillsBrowserAction =
  /** Insert `/<name> ` into the composer. */
  | { readonly kind: "insert"; readonly name: string }
  /** Nothing to run; the detail pane explains the status. */
  | { readonly kind: "inspect" }
  | { readonly kind: "retry" }

export type SkillsInventory =
  | { readonly kind: "loading" }
  | { readonly kind: "ready"; readonly entries: readonly ExtensionInventoryEntry[]; readonly truncated: boolean }
  | { readonly kind: "error"; readonly message: string }

interface SkillsBrowserModelInput {
  readonly inventory: SkillsInventory
  readonly query: string
  readonly selectedId: string | null
}

const TITLE = "SKILLS   /skills"

const SECTIONS: readonly { readonly kind: ExtensionArtifactKind; readonly label: string }[] = [
  { kind: "skill", label: "Skills" },
  { kind: "command", label: "Commands" },
  { kind: "agent", label: "Agents" },
  { kind: "workflow", label: "Workflows" },
  { kind: "mode", label: "Modes" },
  { kind: "hook", label: "Hooks" },
]

/** List-detail projection of the engine's declarative extension inventory. */
export function createSkillsBrowserModel(
  input: SkillsBrowserModelInput,
): ListDetailPresentation<SkillsBrowserAction> {
  const query = input.query.trim()
  if (input.inventory.kind === "loading") {
    return { title: TITLE, query, rows: [], selectedId: null, status: "Loading skills · Esc close", emptyCopy: "Loading skills", notice: null }
  }
  if (input.inventory.kind === "error") {
    const rows: ListDetailRow<SkillsBrowserAction>[] = [{
      kind: "item",
      id: "skills.retry",
      label: "Retry loading skills",
      matchSpans: [],
      detail: { title: "Skills could not be loaded", meta: "", description: input.inventory.message },
      action: { kind: "retry" },
    }]
    return { title: TITLE, query, rows, selectedId: "skills.retry", status: "Enter retry · Esc close", emptyCopy: input.inventory.message, notice: { message: input.inventory.message, tone: "error" } }
  }
  const rows: ListDetailRow<SkillsBrowserAction>[] = []
  for (const section of SECTIONS) {
    const items = input.inventory.entries
      .filter((entry) => entry.kind === section.kind)
      .map((entry, index) => inventoryRow(entry, index))
      .filter((row) => query.length === 0 || fuzzyMatch(query, `${section.label} ${row.label} ${row.detail.meta} ${row.detail.description}`) !== null)
    if (items.length === 0) continue
    rows.push({ kind: "section", id: `skills.section.${section.kind}`, label: section.label })
    rows.push(...items)
  }
  const items = rows.filter((row): row is ListDetailItemRow<SkillsBrowserAction> => row.kind === "item")
  const selected = items.find((row) => row.id === input.selectedId) ?? items[0]
  const counts = countStatuses(input.inventory.entries)
  const summary = `${counts.loaded} loaded${counts.attention > 0 ? ` · ${counts.attention} need attention` : ""}${input.inventory.truncated ? " · list truncated" : ""}`
  return {
    title: TITLE,
    query,
    rows,
    selectedId: selected?.id ?? null,
    status: `${selected?.action.kind === "insert" ? "Enter insert" : "Read only"} · ${summary} · Esc close`,
    emptyCopy: input.inventory.entries.length === 0
      ? "No skills, commands, or agents found in .agents, .rottweiler, or .claude"
      : "No matching skills",
    notice: null,
  }
}

function inventoryRow(entry: ExtensionInventoryEntry, index: number): ListDetailItemRow<SkillsBrowserAction> {
  const name = entry.name ?? fileName(entry.source_path)
  const invocable = entry.name !== null && isLoaded(entry.status) && (entry.kind === "skill" || entry.kind === "command")
  return {
    kind: "item",
    id: `skills.${entry.kind}.${index}.${entry.source_path}`,
    label: `${statusGlyph(entry.status)} ${name}  ${entry.kind} · ${entry.scope}${isLoaded(entry.status) ? "" : ` · ${statusLabel(entry.status)}`}`,
    disabled: false,
    matchSpans: [],
    detail: {
      title: entry.name === null ? name : `/${entry.name}`,
      meta: `${entry.kind} · ${entry.scope} · ${statusLabel(entry.status)}`,
      description: [
        ...(entry.description.length === 0 ? [] : [entry.description, ""]),
        `status     ${statusLabel(entry.status)}`,
        `source     ${entry.scope} · ${entry.location}`,
        `path       ${entry.source_path}`,
        ...entry.notes.map((note, noteIndex) => `${noteIndex === 0 ? "reason     " : "           "}${note}`),
        ...remedy(entry),
        ...(invocable ? ["", `Enter inserts /${entry.name} into the composer.`] : []),
      ].join("\n"),
    },
    action: invocable ? { kind: "insert", name: entry.name! } : { kind: "inspect" },
  }
}

function remedy(entry: ExtensionInventoryEntry): readonly string[] {
  switch (entry.status) {
    case "skipped": return ["", "Fix the file named above, then start a new session to reload it."]
    case "shadowed": return ["", "Rename one of the two artifacts, or remove the higher-precedence copy."]
    case "untrusted": return ["", "Trust this folder with /permissions trust, then start a new session."]
    default: return []
  }
}

function isLoaded(status: ExtensionArtifactStatus): boolean {
  return status === "loaded" || status === "loaded_with_warnings"
}

export function statusLabel(status: ExtensionArtifactStatus): string {
  switch (status) {
    case "loaded": return "loaded"
    case "loaded_with_warnings": return "loaded with warnings"
    case "skipped": return "skipped"
    case "shadowed": return "shadowed"
    case "untrusted": return "untrusted"
  }
}

function statusGlyph(status: ExtensionArtifactStatus): string {
  switch (status) {
    case "loaded": return "✓"
    case "loaded_with_warnings": return "!"
    case "skipped": return "✗"
    case "shadowed": return "◌"
    case "untrusted": return "⊘"
  }
}

function countStatuses(entries: readonly ExtensionInventoryEntry[]): { readonly loaded: number; readonly attention: number } {
  const loaded = entries.filter((entry) => isLoaded(entry.status)).length
  return { loaded, attention: entries.length - loaded + entries.filter((entry) => entry.status === "loaded_with_warnings").length }
}

function fileName(path: string): string {
  const segments = path.split(/[\\/]/u).filter((segment) => segment.length > 0)
  return segments.at(-1) ?? path
}
