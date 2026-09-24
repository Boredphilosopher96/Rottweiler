import type { PickerScreenOptions, PickerScreenRenderable, TextPromptOptions } from "./components/picker"
import type { PickerItem } from "./components"
import { ClientAllocationError, type ClientAllocationOwner, type ClientAllocationLease } from "./client-allocation"
import { retainedJsonBytes } from "./retained-json"

const MAX_PICKER_PAYLOAD_BYTES = 16 * 1024 * 1024
interface PickerPayload { allocation: ClientAllocationLease; references: number }
function releasePayload(value: PickerPayload): void { if (--value.references === 0) value.allocation.release() }

export type PickerKind =
  | "palette" | "keyboardHelp" | "files" | "attachments" | "mcp"
  | "mcpActions" | "mcpInput" | "mcpRemoveConfirm"
  | "modes" | "models" | "providers" | "providerAuth" | "providerApiKey"
  | "providerRecovery" | "providerSetup"
  | "permissions" | "permissionMode" | "permissionYoloConfirm" | "trust"
  | "permissionInput"
  | "queuedMessages"
  | "exportFormat" | "exportPath" | "exportOverwrite"
  | "workspaceRoots"
  | "budgets" | "budgetPresets" | "budgetInput"
  | "sessions" | "sessionRename" | "settings" | "settingChoices"
  | "agents" | "agentActions" | "skills"
  | "timeline" | "timelineActions"
  | "themes" | "uiPanels" | "context" | "contextItems" | "cost" | "errors"

export type PickerCloseReason = "dismiss" | "scope_change"

interface PickerControllerOptions {
  readonly allocations: ClientAllocationOwner
  readonly picker: () => PickerScreenRenderable<unknown>
  readonly terminalWidth: () => number
  readonly terminalHeight: () => number
  readonly vim: () => boolean
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
    this.#options.picker().beginScreen(query)
    this.position(anchored)
    this.#replace(kind)
  }

  /** Esc on a nested screen returns to its parent; false when Esc should close. */
  back(): boolean {
    const picker = this.#options.picker()
    const back = picker.visible ? picker.back : undefined
    if (back === undefined) return false
    back()
    return true
  }

  refresh(): void {
    this.#options.renderPicker(this.kind)
  }

  show<T>(
    title: string,
    items: readonly PickerItem<T>[],
    onSelect: (item: PickerItem<T>) => void,
    screen: PickerScreenOptions<T> = {},
  ): void {
    if (this.#failedPayload !== null) throw new ClientAllocationError("picker replacement requires teardown after a failed render")
    // Include item values and room for filtering, option strings and native text copies.
    const bytes = retainedJsonBytes({ title, items }, MAX_PICKER_PAYLOAD_BYTES / 4) * 4
    if (bytes > MAX_PICKER_PAYLOAD_BYTES) throw new ClientAllocationError("picker payload exceeds its retained allowance")
    const payload: PickerPayload = { allocation: this.#options.allocations.reserve("live", bytes), references: 1 }
    const picker = this.#options.picker()
    const interaction = this.interaction
    // Captured actions (Enter and chords) retire with this revision. Esc back
    // stays live: it only navigates, and must never leave Esc inert.
    const guard = <A extends unknown[]>(action: (...args: A) => void) => (...args: A) => {
      if (!interaction?.active || this.#failedPayload !== null || this.#payload !== payload) return
      payload.references++
      try { action(...args) } finally { releasePayload(payload) }
    }
    const options: PickerScreenOptions<unknown> = {
      ...screen as PickerScreenOptions<unknown>,
      ...(screen.keys === undefined ? {} : {
        keys: screen.keys.map(key => ({ ...key, run: guard(key.run) }) as NonNullable<PickerScreenOptions<unknown>["keys"]>[number]),
      }),
    }
    try {
      this.#options.withRefreshGuard(this.kind, () => {
        picker.vim = this.#options.vim()
        picker.present(`${this.kind ?? "picker"}:${screen.view ?? ""}`, title, items as readonly PickerItem<unknown>[],
          guard(onSelect as (item: PickerItem<unknown>) => void), options, this.#anchored, this.#query)
        this.position(this.#anchored)
        if (this.#anchored) this.#options.focusComposer()
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

  /** Text entry inside the current screen; `back` makes Esc return instead of close. */
  openTextPrompt(options: TextPromptOptions, back?: () => void): void {
    this.#anchored = false
    this.#clearPayload(() => {
      const picker = this.#options.picker()
      picker.vim = this.#options.vim()
      picker.openTextPrompt(options, back)
    })
    this.position(false)
    this.#options.onModalOpened()
  }

  /** Masked credential entry; the value never enters the retained payload. */
  openSecret(title: string, onSubmit: (secret: string) => void): void {
    this.#anchored = false
    this.#clearPayload(() => {
      const picker = this.#options.picker()
      picker.vim = this.#options.vim()
      picker.openSecret(title, onSubmit)
    })
    this.position(false)
    this.#options.onModalOpened()
  }

  showLoading(title: string, message: string): void {
    this.#clearPayload(() => this.#options.picker().showLoading(title, message, this.#anchored))
    this.position(this.#anchored)
    if (this.#anchored) this.#options.focusComposer()
    else this.#options.onModalOpened()
  }

  showStatus(title: string, message: string, description: string): void {
    this.#clearPayload(() => this.#options.picker().showStatus(title, message, description, this.#anchored))
    this.position(this.#anchored)
    if (this.#anchored) this.#options.focusComposer()
    else this.#options.onModalOpened()
  }

  close(reason: PickerCloseReason = "dismiss"): void {
    const kind = this.kind
    this.kind = null
    this.#clearPayload(() => this.#options.picker().close())
    this.#anchored = false
    this.#query = ""
    this.#options.onClosed(kind, reason)
  }

  /**
   * A modal screen owns the primary area above the status line and composer,
   * so no transcript shows around it. The `@` list sits directly above the
   * composer instead.
   */
  position(anchored = this.#anchored): void {
    const picker = this.#options.picker()
    const width = this.#options.terminalWidth()
    const height = this.#options.terminalHeight()
    const primary = Math.max(1, height - this.#options.statusHeight() - this.#options.composerDockHeight())
    if (anchored) {
      const rows = Math.max(2, Math.min(picker.desiredInlineRows, primary))
      picker.resizeInline(width, Math.max(0, primary - rows), rows)
    } else {
      picker.resizeForTerminal(width, height, Math.max(6, primary))
      if (picker.mode === "status") picker.input.visible = false
    }
  }
}
