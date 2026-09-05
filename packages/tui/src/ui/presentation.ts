import type { UiDisplayField, UiPresentation, UiProjectedField } from "../protocol"

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
  return {
    presentation,
    fields: presentation.descriptor.fields.map(field => {
      const value = values.get(field.id)
      if (value === undefined || value.kind !== field.kind) throw new Error("presentation field identity mismatch")
      return { id: field.id, kind: field.kind, text: fieldRenderers[field.kind](field, value) }
    }),
  }
}
