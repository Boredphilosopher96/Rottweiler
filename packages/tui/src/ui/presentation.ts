import type { UiDisplayField, UiPresentation, UiProjectedField, UiPanels } from "../protocol"

export interface UiRenderedField {
  readonly id: string
  readonly kind: UiDisplayField["kind"]
  readonly text: string
}
export interface UiSurfaceModel {
  readonly presentation: UiPresentation
  readonly fields: readonly UiRenderedField[]
}

type FieldRenderer = (field: UiDisplayField, value: UiProjectedField) => string
/** Built-in and extension descriptors use the same closed data-only field renderers. */
const fieldRenderers: Readonly<Record<UiDisplayField["kind"], FieldRenderer>> = {
  text: (field, value) => `${field.label}\n${value.kind === "text" ? value.value ?? "—" : "—"}`,
  badge: (field, value) => `${field.label}  [${value.kind === "badge" ? value.value ?? "—" : "—"}]`,
  list: (field, value) => `${field.label}\n${value.kind === "list" ? value.values.map(item => `• ${item}`).join("\n") : "—"}`,
  table: (field, value) => field.kind === "table" && value.kind === "table"
    ? `${field.label}\n${field.columns.join(" │ ")}\n${value.rows.map(row => row.map(cell => cell || "—").join(" │ ")).join("\n")}` : "—",
}

/** Build presentation strings once under the decoded source's allocation reservation. */
export function prepareUiSurface(presentation: UiPresentation): UiSurfaceModel {
  if (presentation.projected.fields.length !== presentation.descriptor.fields.length) throw new Error("presentation field count mismatch")
  const values = new Map(presentation.projected.fields.map(field => [field.id, field]))
  if (values.size !== presentation.projected.fields.length
    || new Set(presentation.descriptor.fields.map(field => field.id)).size !== presentation.descriptor.fields.length) {
    throw new Error("duplicate presentation field identity")
  }
  return {
    presentation,
    fields: presentation.descriptor.fields.map(field => {
      const value = values.get(field.id)
      if (value === undefined || value.kind !== field.kind) throw new Error("presentation field identity mismatch")
      if (field.kind === "list" && value.kind === "list" && value.values.length > field.max_items) throw new Error("presentation list bound mismatch")
      if (field.kind === "table" && value.kind === "table"
        && (value.rows.length > field.max_rows || value.rows.some(row => row.length !== field.columns.length))) {
        throw new Error("presentation table bound mismatch")
      }
      return { id: field.id, kind: field.kind, text: fieldRenderers[field.kind](field, value) }
    }),
  }
}

export interface UiPanelModel {
  readonly revision: number
  readonly model: UiSurfaceModel
}
export function prepareUiPanels(value: UiPanels): readonly UiPanelModel[] {
  const identities = new Set<string>()
  return value.panels.map(panel => {
    const identity = uiIdentity(panel.presentation)
    if (panel.presentation.descriptor.surface.surface !== "panel" || identities.has(identity)) throw new Error("panel identity mismatch")
    identities.add(identity)
    return { revision: panel.revision, model: prepareUiSurface(panel.presentation) }
  })
}
export function uiIdentity(value: Pick<UiPresentation, "owner" | "descriptor">): string {
  return JSON.stringify([value.owner.extension, value.owner.generation, value.descriptor.id])
}
