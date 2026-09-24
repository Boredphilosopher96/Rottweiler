import type { PickerItem } from "../components"
import type { PickerController } from "../picker-controller"
import { presentError } from "../render/errors"
import type { RottweilerState } from "../state"
import type { ErrorHistoryEntry } from "../state/errors"
import { truncateToCells } from "../render/text"

interface ErrorHost {
  readonly ui: { readonly state: RottweilerState; setState(state: RottweilerState): void }
  readonly pickerController: PickerController
  readonly terminalWidth: number
}

/**
 * Errors: failures retained for this session, newest first. The detail pane
 * shows the full message; Enter opens it line by line so narrow terminals
 * keep every word reachable. Dismissing the banner is explicit.
 */
export class ErrorUiController {
  #selectedId: number | null = null
  #lastViewed: number | null = null
  constructor(readonly host: ErrorHost) {}
  open(): void {
    this.#selectedId = null
    this.#lastViewed = null
    this.host.pickerController.begin("errors")
    this.host.pickerController.refresh()
  }
  render(): void {
    const history = this.host.ui.state.errorHistory
    const selected = history.find(error => error.id === this.#selectedId)
    if (selected !== undefined) {
      this.#renderDetails(selected)
      return
    }
    if (history.length === 0 && this.host.ui.state.errors.length === 0) {
      this.host.pickerController.showStatus("ERRORS   /errors", "No errors in this session", "Failures are kept here, newest first, until the session ends.")
      return
    }
    const items: PickerItem<number>[] = [...history].reverse().map(error => {
      const presented = presentError(error)
      return {
        id: `error:${error.id}`,
        label: presented.text,
        hint: error.retryable ? "retryable" : presented.severity,
        tone: presented.severity === "error" ? "error" as const : "warning" as const,
        description: `${error.category} · ${error.code}`,
        detail: `${presented.text}\n\n${error.message}\n\n${error.category} · ${error.code}\n${nextStep(error)}`,
        primary: "details",
        value: error.id,
      }
    })
    const notices = this.host.ui.state.errors.length
    this.host.pickerController.show(`ERRORS   ${history.length} retained   /errors`, items, item => {
      this.#selectedId = item.value
      this.host.pickerController.refresh()
    }, {
      selectedId: this.#lastViewed === null ? null : `error:${this.#lastViewed}`,
      keys: [{
        stroke: "ctrl+d", label: "dismiss notices", available: () => this.host.ui.state.errors.length > 0,
        run: () => {
          this.host.ui.setState({ ...this.host.ui.state, errors: [] })
          this.host.pickerController.refresh()
        },
      }],
      notice: notices === 0 ? null : { message: `${notices} ${notices === 1 ? "notice" : "notices"} on screen`, tone: "warning" },
    })
  }

  #renderDetails(error: ErrorHistoryEntry): void {
    const presented = presentError(error)
    const rows: PickerItem<null>[] = [
      { id: "summary", label: presented.text, hint: presented.severity, description: `${error.category} · ${error.code}`, primary: null, value: null },
      { id: "next", label: error.retryable ? "Retryable" : "Needs attention", description: nextStep(error), primary: null, value: null },
      { id: "section.message", label: "Message", description: "", sectionHeader: true, value: null },
    ]
    // Each bounded line is keyboard-reachable even on the narrow single-pane screen.
    let remaining = error.message
    for (let index = 0; remaining.length > 0; index++) {
      const line = truncateToCells(remaining, Math.max(12, this.host.terminalWidth - 10))
      const length = line.endsWith("…") ? line.length - 1 : line.length
      const text = remaining.slice(0, Math.max(1, length))
      rows.push({ id: `message:${index}`, label: text, description: error.message, primary: null, value: null })
      remaining = remaining.slice(text.length)
    }
    this.host.pickerController.show(`ERRORS › Error ${error.id}`, rows, () => {}, {
      view: `error:${error.id}`,
      back: () => {
        this.#lastViewed = error.id
        this.#selectedId = null
        this.host.pickerController.refresh()
      },
    })
  }
}

function nextStep(error: ErrorHistoryEntry): string {
  return error.retryable ? "Retry the original action when ready." : "Review these details before continuing."
}
