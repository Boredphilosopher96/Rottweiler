import type { KeyEvent } from "@opentui/core"
import type { ListDetailPresentation, ListDetailRenderable } from "../components"
import type { PickerController } from "../picker-controller"

/** Layout and lifecycle a full-primary navigation screen shares with the app. */
export interface ScreenShell {
  readonly pickerController: PickerController
  readonly terminalWidth: number
  readonly terminalHeight: number
  readonly statusHeight: number
  readonly composerDockHeight: number
  readonly vim: boolean
  closePicker(): void
  modalOpened(): void
}

/** Fits a screen to the primary area above the status line and composer dock. */
export function resizeScreen<Action>(
  shell: ScreenShell,
  browser: ListDetailRenderable<Action>,
  width: number,
  height: number,
): void {
  browser.resizeForTerminal(width, height, Math.max(6, height - shell.statusHeight - shell.composerDockHeight))
}

/** Opens the screen on first render and refreshes it in place afterwards. */
export function presentScreen<Action>(
  shell: ScreenShell,
  browser: ListDetailRenderable<Action>,
  presentation: ListDetailPresentation<Action>,
  onSelect: (action: Action) => void,
  onKey?: (key: KeyEvent) => boolean,
): void {
  const shown = shell.vim ? { ...presentation, status: presentation.status.replace("Esc close", "Esc×2 close") } : presentation
  if (browser.visible) {
    browser.refresh(shown)
    return
  }
  browser.open(shown, onSelect, {
    onQuery: () => shell.pickerController.refresh(),
    onSelection: () => shell.pickerController.refresh(),
    ...(onKey === undefined ? {} : { onKey }),
  })
  shell.modalOpened()
}

/** Current filter text, preserved across refreshes and theme rebuilds. */
export function screenQuery<Action>(shell: ScreenShell, browser: ListDetailRenderable<Action>): string {
  const query = browser.visible ? browser.input.value : shell.pickerController.query
  shell.pickerController.query = query
  return query
}
