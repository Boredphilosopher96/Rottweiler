import type { PickerItem } from "../components"
import type { PickerController } from "../picker-controller"
import { presentError } from "../render/errors"
import type { RottweilerState } from "../state"
import { truncateToCells } from "../render/text"

interface ErrorHost {
  readonly ui: { readonly state: RottweilerState; setState(state: RottweilerState): void }
  readonly pickerController: PickerController
  readonly terminalWidth: number
}
export class ErrorUiController {
  #selectedId: number | null = null
  constructor(readonly host: ErrorHost) {}
  open(): void {
    this.#selectedId = null
    this.host.pickerController.begin("errors")
    this.host.pickerController.refresh()
  }
  render(): void {
    const history = this.host.ui.state.errorHistory
    const selected = history.find(error => error.id === this.#selectedId)
    if (selected !== undefined) {
      const description = presentError(selected)
      const rows: PickerItem<null>[] = [
        { id: "back", label: "Back to recent errors", description: "Retained for this session · no automatic retry", value: null },
        { id: "summary", label: description.text, description: `${description.severity} · ${selected.category} · ${selected.code}`, value: null },
        { id: "retryability", label: selected.retryable ? "Retryable" : "Requires attention", description: selected.retryable ? "Retry the original action when ready" : "Review these details before continuing", value: null },
      ]
      // Each bounded line is keyboard-reachable even on the narrow single-pane screen.
      let remaining = selected.message
      for (let index = 0; remaining.length > 0; index++) {
        const line = truncateToCells(remaining, Math.max(12, this.host.terminalWidth - 10))
        const length = line.endsWith("…") ? line.length - 1 : line.length
        const text = remaining.slice(0, Math.max(1, length))
        rows.push({ id: `message:${index}`, label: text, description: "", value: null })
        remaining = remaining.slice(text.length)
      }
      this.host.pickerController.show(`Error ${selected.id} · details`, rows, item => { if (item.id === "back") this.open() })
      return
    }
    if (history.length === 0 && this.host.ui.state.errors.length === 0) {
      this.host.pickerController.showStatus("Recent errors", "No retained errors", "This session has no retained failures.")
      return
    }
    const items: PickerItem<number | null>[] = [
      ...(this.host.ui.state.errors.length === 0 ? [] : [{ id: "dismiss", label: "Dismiss current notices", description: "Clear the banner; retain error details below", value: null }]),
      ...[...history].reverse().map(error => ({
        id: `error:${error.id}`, label: presentError(error).text,
        description: `${error.category} · ${error.code} · ${error.message}`, value: error.id,
      })),
    ]
    this.host.pickerController.show("Recent errors · newest first · last 64", items, item => {
      if (item.id === "dismiss") this.host.ui.setState({ ...this.host.ui.state, errors: [] })
      else this.#selectedId = item.value
      this.host.pickerController.refresh()
    })
  }
}
