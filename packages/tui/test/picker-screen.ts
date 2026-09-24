import type { PickerScreenRenderable } from "../src/components"

/** Listed rows of the generic screen, as `{ name, description, value }` with the row id as value. */
export function options<T>(picker: PickerScreenRenderable<T>): { name: string; description: string; value: string }[] {
  return picker.items.map(item => ({ name: item.label, description: item.description, value: item.id }))
}

/** Select the listed row at `index` (section headers are not rows). */
export function select<T>(picker: PickerScreenRenderable<T>, index: number): void {
  const item = picker.items[index]
  if (item === undefined) throw new Error(`no screen row at ${index}: ${JSON.stringify(picker.items.map(row => row.id))}`)
  picker.selectById(item.id)
}

/** Select the first row whose label contains `label`, then press Enter. */
export function choose<T>(picker: PickerScreenRenderable<T>, label: string): void {
  const item = picker.items.find(row => row.label.includes(label))
  if (item === undefined) throw new Error(`no screen row labelled ${label}: ${JSON.stringify(picker.items.map(row => row.label))}`)
  picker.selectById(item.id)
  picker.activateSelected()
}

export function selectedOption<T>(picker: PickerScreenRenderable<T>): { name: string; value: string } | null {
  const item = picker.selectedItem
  return item === null ? null : { name: item.label, value: item.id }
}

/** The message of a status or prompt screen, as rendered in its list area. */
export function statusText<T>(picker: PickerScreenRenderable<T>): string {
  return picker.rowViews.map(view => view.plainText.trim()).filter(line => line.length > 0).join("\n")
}
