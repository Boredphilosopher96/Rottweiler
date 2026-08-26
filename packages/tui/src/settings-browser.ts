import { fuzzyMatch } from "./components/picker"
import type {
  ListDetailItemRow,
  ListDetailPresentation,
  ListDetailRow,
} from "./components/list-detail"
import type { RottweilerState } from "./state/model"

type Setting = RottweilerState["settings"][number]

export type SettingsSectionId =
  | "model"
  | "permissions"
  | "compaction"
  | "budget"
  | "mcp"
  | "appearance"
  | "keybindings"
  | "other"

export type SettingsBrowserAction =
  | { readonly kind: "choose"; readonly key: string }
  | { readonly kind: "openThemes" }
  | { readonly kind: "openBudgets" }
  | { readonly kind: "inspect" }

export type SettingsCatalog =
  | { readonly kind: "loading" }
  | { readonly kind: "ready"; readonly settings: RottweilerState["settings"] }
  | {
      readonly kind: "error"
      readonly message: string
      readonly stale: RottweilerState["settings"]
    }

interface SectionDefinition {
  readonly id: SettingsSectionId
  readonly label: string
  readonly includes: (key: string) => boolean
}

const SECTIONS: readonly SectionDefinition[] = [
  { id: "model", label: "Model & routing", includes: (key) => key === "project.models.default" || key.startsWith("models.thinking.") },
  { id: "permissions", label: "Permissions", includes: (key) => key.startsWith("permissions.") },
  { id: "compaction", label: "Context & compaction", includes: (key) => key.startsWith("compaction.") },
  { id: "budget", label: "Budget & guardrails", includes: (key) => key.startsWith("budget.") },
  { id: "mcp", label: "MCP servers", includes: (key) => key.startsWith("mcp.servers.") },
  { id: "appearance", label: "Appearance", includes: (key) => key === "ui.theme" },
  { id: "keybindings", label: "Keybindings", includes: (key) => key === "ui.keybindings.preset" },
  { id: "other", label: "Other", includes: () => true },
]

interface SettingsBrowserModelInput {
  readonly catalog: SettingsCatalog
  readonly query: string
  readonly selectedId: string | null
}

export function createSettingsBrowserModel(
  input: SettingsBrowserModelInput,
): ListDetailPresentation<SettingsBrowserAction> {
  const query = input.query.trim()
  if (input.catalog.kind === "loading") {
    return emptyPresentation("Loading settings", "Loading settings")
  }

  const settings = input.catalog.kind === "ready"
    ? input.catalog.settings
    : input.catalog.stale
  const rows = settingsRows(settings, query)
  const selectedId = retainedSelection(rows, input.selectedId)
  const selected = rows.find(
    (row): row is ListDetailItemRow<SettingsBrowserAction> =>
      row.kind === "item" && row.id === selectedId,
  )
  const emptyCopy = input.catalog.kind === "error" && settings.length === 0
    ? input.catalog.message
    : "No matching settings"
  const status = input.catalog.kind === "error" && settings.length === 0
    ? "Ctrl-R retry"
    : `${actionHint(selected?.action)} · Esc close`

  return {
    title: "SETTINGS   /settings",
    query,
    rows,
    selectedId,
    status,
    emptyCopy,
    notice: input.catalog.kind === "error"
      ? { message: input.catalog.message, tone: "error" }
      : null,
  }
}

function emptyPresentation(
  emptyCopy: string,
  status: string,
): ListDetailPresentation<SettingsBrowserAction> {
  return {
    title: "SETTINGS   /settings",
    query: "",
    rows: [],
    selectedId: null,
    status,
    emptyCopy,
    notice: null,
  }
}

function settingsRows(
  settings: RottweilerState["settings"],
  query: string,
): readonly ListDetailRow<SettingsBrowserAction>[] {
  const rows: ListDetailRow<SettingsBrowserAction>[] = []
  for (const section of SECTIONS) {
    const sectionSettings = settings.filter(
      (setting) => sectionFor(setting.key).id === section.id,
    )
    const items = sectionItems(section.id, sectionSettings).filter((row) =>
      matchesQuery(row, section.label, query)
    )
    if (items.length === 0) continue
    rows.push({ kind: "section", id: `settings.section.${section.id}`, label: section.label })
    rows.push(...items)
  }
  return rows
}

function sectionItems(
  section: SettingsSectionId,
  settings: readonly Setting[],
): readonly ListDetailItemRow<SettingsBrowserAction>[] {
  if (section === "budget" && settings.length > 0) {
    return [{
      kind: "item",
      id: "budget.manage",
      label: "Budget limits",
      matchSpans: [],
      detail: {
        title: "Budget limits",
        meta: "budget.*",
        description: `${settings.length} engine settings\nOpen the dedicated Budget limits picker.`,
      },
      action: { kind: "openBudgets" },
    }]
  }

  return settings.flatMap((setting) => {
    if (setting.key === "ui.theme") {
      return [routeRow(setting, "ui.theme", { kind: "openThemes" })]
    }
    if (setting.key === "project.models.default") {
      return [routeRow(setting, "project.models.default", { kind: "inspect" })]
    }
    return setting.choices.length === 0
      ? [routeRow(setting, setting.key, { kind: "inspect" })]
      : [settingRow(setting)]
  })
}

function routeRow(
  setting: Setting,
  id: string,
  action: Exclude<SettingsBrowserAction, { readonly kind: "choose" }>,
): ListDetailItemRow<SettingsBrowserAction> {
  return {
    kind: "item",
    id,
    label: setting.label,
    matchSpans: [],
    detail: settingDetail(setting),
    action,
  }
}

function settingRow(
  setting: Setting,
): ListDetailItemRow<SettingsBrowserAction> {
  return {
    kind: "item",
    id: setting.key,
    label: setting.label,
    matchSpans: [],
    detail: {
      title: setting.label,
      meta: setting.key,
      description: [
        `current    ${setting.value}`,
        `choices    ${setting.choices.join(" · ")}`,
        `source     ${setting.provenance}`,
        `applies    ${setting.appliesImmediately ? "live" : "next session"}`,
      ].join("\n"),
    },
    action: { kind: "choose", key: setting.key },
  }
}

function settingDetail(
  setting: Setting,
): ListDetailItemRow<SettingsBrowserAction>["detail"] {
  return {
    title: setting.label,
    meta: setting.key,
    description: [
      `current    ${setting.value}`,
      `source     ${setting.provenance}`,
      `applies    ${setting.appliesImmediately ? "live" : "next session"}`,
    ].join("\n"),
  }
}

function sectionFor(key: string): SectionDefinition {
  return SECTIONS.find((section) => section.includes(key)) ?? SECTIONS[SECTIONS.length - 1]!
}

function matchesQuery(
  row: ListDetailItemRow<SettingsBrowserAction>,
  section: string,
  query: string,
): boolean {
  if (query.length === 0) return true
  return fuzzyMatch(query, `${section} ${row.label} ${row.detail.meta} ${row.detail.description}`) !== null
}

function retainedSelection(
  rows: readonly ListDetailRow<SettingsBrowserAction>[],
  requested: string | null,
): string | null {
  if (requested !== null && rows.some((row) => row.kind === "item" && row.id === requested)) {
    return requested
  }
  return rows.find((row): row is ListDetailItemRow<SettingsBrowserAction> => row.kind === "item")?.id ?? null
}

function actionHint(action: SettingsBrowserAction | undefined): string {
  if (action?.kind === "choose") return "Enter choose"
  if (action?.kind === "openThemes" || action?.kind === "openBudgets") return "Enter open"
  return "Read only"
}
