import type { FuzzyPickerRenderable, PickerItem } from "./components"

export type PickerKind =
  | "palette" | "keyboardHelp" | "commands" | "files" | "attachments" | "mcp"
  | "mcpActions" | "mcpInput" | "mcpRemoveConfirm"
  | "modes" | "models" | "providers" | "providerAuth" | "providerApiKey"
  | "providerRecovery"
  | "permissions" | "permissionMode" | "permissionYoloConfirm" | "trust"
  | "permissionInput"
  | "queuedMessages"
  | "exportFormat" | "exportPath" | "exportOverwrite"
  | "workspaceRoots"
  | "budgets" | "budgetPresets" | "budgetInput"
  | "sessions" | "sessionActions" | "sessionRename" | "settings" | "settingChoices"
  | "agents" | "agentActions"
  | "timeline" | "timelineActions"
  | "themes"

export type PickerCloseReason = "dismiss" | "scope_change"

interface PickerControllerOptions {
  readonly picker: () => FuzzyPickerRenderable<unknown>
  readonly terminalHeight: () => number
  readonly statusHeight: () => number
  readonly composerDockHeight: () => number
  readonly focusComposer: () => void
  readonly renderPicker: (kind: PickerKind | null) => void
  readonly withRefreshGuard: (kind: PickerKind | null, refresh: () => void) => void
  readonly onModalOpened: () => void
  readonly onClosed: (kind: PickerKind | null, reason: PickerCloseReason) => void
}

export interface PickerInteraction {
  readonly active: boolean
  onRetire(cleanup: () => void): void
}

class OwnedPickerInteraction implements PickerInteraction {
  #active = true
  #cleanup: (() => void) | null = null
  get active(): boolean { return this.#active }
  onRetire(cleanup: () => void): void {
    if (!this.#active) { cleanup(); return }
    if (this.#cleanup !== null) throw new Error("picker interaction already has a cleanup owner")
    this.#cleanup = cleanup
  }
  retire(): void {
    if (!this.#active) return
    this.#active = false
    const cleanup = this.#cleanup
    this.#cleanup = null
    cleanup?.()
  }
}

export class PickerController {
  readonly #options: PickerControllerOptions
  #active: { readonly kind: PickerKind; readonly interaction: OwnedPickerInteraction } | null = null
  #anchored = false
  #query = ""

  constructor(options: PickerControllerOptions) {
    this.#options = options
  }

  get kind(): PickerKind | null {
    return this.#active?.kind ?? null
  }

  set kind(kind: PickerKind | null) {
    if (kind !== this.kind) this.#replace(kind)
  }

  get interaction(): PickerInteraction | null { return this.#active?.interaction ?? null }

  #replace(kind: PickerKind | null): void {
    const previous = this.#active
    this.#active = kind === null ? null : { kind, interaction: new OwnedPickerInteraction() }
    previous?.interaction.retire()
  }

  dispose(): void { this.#replace(null) }

  get anchored(): boolean {
    return this.#anchored
  }

  set anchored(anchored: boolean) {
    this.#anchored = anchored
  }

  get query(): string {
    return this.#query
  }

  set query(query: string) {
    this.#query = query
  }

  begin(kind: PickerKind, anchored = false, query = ""): void {
    this.#anchored = anchored
    this.#query = query
    this.position(anchored)
    this.#replace(kind)
  }

  refresh(): void {
    this.#options.renderPicker(this.kind)
  }

  show<T>(
    title: string,
    items: readonly PickerItem<T>[],
    onSelect: (item: PickerItem<T>) => void,
  ): void {
    const picker = this.#options.picker()
    const interaction = this.interaction
    const select = (item: PickerItem<unknown>) => {
      if (interaction?.active) onSelect(item as PickerItem<T>)
    }
    this.#options.withRefreshGuard(this.kind, () => {
      if (this.#anchored) {
        picker.refreshAnchored(
          title,
          items as readonly PickerItem<unknown>[],
          this.#query,
          select,
        )
        this.position(true)
        this.#options.focusComposer()
      } else {
        picker.refresh(title, items as readonly PickerItem<unknown>[], select, false)
        this.position(false)
      }
    })
    if (!this.#anchored) this.#options.onModalOpened()
  }

  showLoading(title: string, message: string): void {
    this.#options.picker().showLoading(title, message, this.#anchored)
    this.position(this.#anchored)
    if (this.#anchored) this.#options.focusComposer()
  }

  showStatus(title: string, message: string, description: string): void {
    this.#options.picker().showStatus(title, message, description, this.#anchored)
    this.position(this.#anchored)
    if (this.#anchored) this.#options.focusComposer()
  }

  close(reason: PickerCloseReason = "dismiss"): void {
    const kind = this.kind
    this.kind = null
    this.#options.picker().close()
    this.#anchored = false
    this.#query = ""
    this.#options.onClosed(kind, reason)
  }

  position(anchored = this.#anchored): void {
    const picker = this.#options.picker()
    const terminalHeight = this.#options.terminalHeight()
    if (anchored) {
      const composerTop = Math.max(
        0,
        terminalHeight - this.#options.statusHeight() - this.#options.composerDockHeight(),
      )
      const pickerHeight = picker.constrainAnchoredHeight(composerTop)
      picker.bottom = undefined
      picker.top = Math.max(0, composerTop - pickerHeight)
      picker.left = 0
      picker.width = "100%"
    } else {
      const top = Math.min(2, Math.max(0, terminalHeight - 2))
      picker.constrainModalHeight(
        Math.max(1, terminalHeight - top - this.#options.statusHeight()),
      )
      picker.bottom = undefined
      picker.top = top
      picker.left = "15%"
      picker.width = "70%"
    }
  }
}
