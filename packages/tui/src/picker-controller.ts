import type { FuzzyPickerRenderable, PickerItem } from "./components"
import { ClientAllocationError, type ClientAllocationOwner, type ClientAllocationLease } from "./client-allocation"
import { retainedJsonBytes } from "./retained-json"

const MAX_PICKER_PAYLOAD_BYTES = 16 * 1024 * 1024
interface PickerPayload { allocation: ClientAllocationLease; references: number }
function releasePayload(value: PickerPayload): void { if (--value.references === 0) value.allocation.release() }

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
  | "themes" | "uiPanels"

export type PickerCloseReason = "dismiss" | "scope_change"

interface PickerControllerOptions {
  readonly allocations: ClientAllocationOwner
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
  #payload: PickerPayload | null = null
  #failedPayload: PickerPayload | null = null

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

  dispose(): void {
    this.#replace(null)
    this.#clearPayload(() => { const picker = this.#options.picker(); if (!picker.isDestroyed) picker.close() })
  }

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
    if (this.#failedPayload !== null) throw new ClientAllocationError("picker replacement requires teardown after a failed render")
    // Include item values and room for filtering, option strings and native text copies.
    const bytes = retainedJsonBytes({ title, items }, MAX_PICKER_PAYLOAD_BYTES / 4) * 4
    if (bytes > MAX_PICKER_PAYLOAD_BYTES) throw new ClientAllocationError("picker payload exceeds its retained allowance")
    const payload: PickerPayload = { allocation: this.#options.allocations.reserve("live", bytes), references: 1 }
    const picker = this.#options.picker()
    const interaction = this.interaction
    const select = (item: PickerItem<unknown>) => {
      if (!interaction?.active || this.#failedPayload !== null || this.#payload !== payload) return
      payload.references++
      try { onSelect(item as PickerItem<T>) } finally { releasePayload(payload) }
    }
    try {
      this.#options.withRefreshGuard(this.kind, () => {
        if (this.#anchored) {
          picker.refreshAnchored(title, items as readonly PickerItem<unknown>[], this.#query, select)
          this.position(true)
          this.#options.focusComposer()
        } else {
          picker.refresh(title, items as readonly PickerItem<unknown>[], select, false)
          this.position(false)
        }
      })
    } catch (error) {
      // Native replacement may have retained either revision before it failed.
      this.#failedPayload = payload
      throw error
    }
    const previous = this.#payload
    this.#payload = payload
    if (previous !== null) releasePayload(previous)
    if (!this.#anchored) this.#options.onModalOpened()
  }

  #clearPayload(clear: () => void): void {
    clear()
    const previous = this.#payload, failed = this.#failedPayload
    this.#payload = null; this.#failedPayload = null
    if (previous !== null) releasePayload(previous)
    if (failed !== null) releasePayload(failed)
  }

  showLoading(title: string, message: string): void {
    this.#clearPayload(() => this.#options.picker().showLoading(title, message, this.#anchored))
    this.position(this.#anchored)
    if (this.#anchored) this.#options.focusComposer()
  }

  showStatus(title: string, message: string, description: string): void {
    this.#clearPayload(() => this.#options.picker().showStatus(title, message, description, this.#anchored))
    this.position(this.#anchored)
    if (this.#anchored) this.#options.focusComposer()
  }

  close(reason: PickerCloseReason = "dismiss"): void {
    const kind = this.kind
    this.kind = null
    this.#clearPayload(() => this.#options.picker().close())
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
