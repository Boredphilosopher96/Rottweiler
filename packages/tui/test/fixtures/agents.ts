import type { RottweilerApp } from "../../src/app"
import { selectedOption } from "../picker-screen"

/**
 * Opens the selected Agents screen row: a pending-control row enters the
 * child directly; a child row opens its actions, whose first action views it.
 */
export function enterSelectedAgent(app: RottweilerApp): void {
  app.agentsBrowser.activateSelected()
  if (app.picker.visible && selectedOption(app.picker)?.value === "view") app.picker.activateSelected()
}
