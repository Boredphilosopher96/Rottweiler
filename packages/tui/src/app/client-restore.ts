import type { RottweilerApp, PrimaryView } from "../app"
import type { PickerController } from "../picker-controller"
import {
  isRestorablePicker,
  parseTuiRecycleState,
  type AppClientState,
  type ClientComposerState,
} from "../recycle-state"
import type { RottweilerState } from "../state"
import { kennelTheme, themeByName, type RottweilerTheme } from "../theme"
import type { ChildUiController } from "./children"
import type { SessionUiController } from "./sessions"
import type { InputUiController } from "./input"
import type { ProviderUiController } from "./provider"
import type { McpUiController } from "./mcp"
import type { ThemeUiController } from "./themes"
import type { SubmissionController } from "./submission"
import type { PickerContentController } from "./picker-content"
interface ClientRestoreHost {
  readonly ui: Pick<RottweilerApp,
    | "commandPalette"
    | "composer"
    | "interactionPanel"
    | "mcpBrowser"
    | "openAttachmentPicker"
    | "openBudgetPicker"
    | "openCommandPicker"
    | "openKeyboardHelpPicker"
    | "openMcpPicker"
    | "openModePicker"
    | "openModelPicker"
    | "openPermissionModePicker"
    | "openPermissionPicker"
    | "openProviderPicker"
    | "openQueuedMessagesPicker"
    | "openSessionPicker"
    | "openSettingsPicker"
    | "openSubagentPicker"
    | "openThemePicker"
    | "openTimelinePicker"
    | "openTrustPicker"
    | "openWorkspaceRootsPicker"
    | "outputViewer"
    | "picker"
    | "primaryView"
    | "setState"
    | "settingsBrowser"
    | "state"
    | "themeBrowser"
    | "toolsWorkspace"
    | "transcript"
  >
  readonly pickerController: PickerController
  readonly children: ChildUiController
  readonly sessions: SessionUiController
  readonly input: InputUiController
  readonly providers: ProviderUiController
  readonly mcp: McpUiController
  readonly themes: ThemeUiController
  readonly submission: SubmissionController
  readonly pickerContent: PickerContentController
  readonly submissionsInFlight: number
  readonly sessionId: string
  readonly theme: RottweilerTheme
  readonly reviewOpen: boolean
  resolveTheme(theme: RottweilerTheme): RottweilerTheme
  applyTheme(theme: RottweilerTheme): void
  setPrimaryView(view: PrimaryView): void
  updateToolsWorkspace(state: RottweilerState, restoreHidden: boolean): void
}
export class ClientRestoreController {
  #pendingClientState: AppClientState | null = null
  constructor(readonly host: ClientRestoreHost) {}
  discard(): void { this.#pendingClientState = null }
  captureComposerState(): ClientComposerState {
    return {
      content: this.host.ui.composer.value,
      attachments: [...this.host.ui.composer.attachments],
      cursorOffset: this.host.ui.composer.editor.cursorOffset,
      selection: this.host.ui.composer.editor.getSelection(),
    }
  }

  restoreComposerState(state: ClientComposerState): void {
    this.host.ui.composer.restoreDraft(state.content, state.attachments)
    this.host.ui.composer.editor.cursorOffset = state.cursorOffset
    if (state.selection !== null) this.host.ui.composer.editor.setSelection(state.selection.start, state.selection.end)
  }

  clientPickerSurface() {
    switch (this.host.pickerController.kind) {
      case "palette": return this.host.ui.commandPalette
      case "mcp": return this.host.ui.mcpBrowser
      case "settings": return this.host.ui.settingsBrowser
      case "themes": return this.host.ui.themeBrowser
      default: return null
    }
  }

  /** Return no handoff while an interaction needs its current process or cannot fit the private cap. */
  recycleState(): AppClientState | null {
    const kind = this.host.pickerController.kind
    if (this.host.children.activeId !== null || this.host.submissionsInFlight > 0
      || this.host.submission.terminalSuspended || this.host.ui.state.shell.active || this.host.ui.state.replay.active
      || this.host.providers.hasPendingAction
      || this.host.ui.state.providerAuth.pending !== null || this.host.mcp.hasDraft
      || this.host.sessions.pending
      || this.host.reviewOpen || this.host.ui.outputViewer.visible || this.host.ui.interactionPanel.visible
      || (kind !== null && !isRestorablePicker(kind))) return null
    const surface = this.clientPickerSurface()
    const selected = surface?.selectedId ?? this.host.ui.picker.select.getSelectedOption()?.value
    return parseTuiRecycleState({
      schemaVersion: 2,
      sessionId: this.host.sessionId,
      composer: this.captureComposerState(),
      subagentDrafts: [...this.host.children.drafts],
      primaryView: this.host.ui.primaryView,
      scrollTop: Math.max(0, this.host.ui.transcript.scroller.scrollTop),
      toolsScrollTop: Math.max(0, this.host.ui.toolsWorkspace.activityScroller.scrollTop),
      transcript: this.host.ui.transcript.captureClientState(),
      tools: this.host.ui.toolsWorkspace.captureClientState(),
      inputMode: this.host.input.mode,
      focus: this.host.input.focus === "picker" ? this.host.input.beforePicker : this.host.input.focus,
      theme: this.host.theme.name,
      picker: kind === null ? null : {
        kind,
        anchored: this.host.pickerController.anchored,
        query: surface?.input.value ?? (this.host.pickerController.anchored ? this.host.pickerController.query : this.host.ui.picker.input.value),
        selectedId: typeof selected === "string" ? selected : null,
        scrollOffset: surface?.scrollOffset ?? 0,
        modelProviderFilter: this.host.providers.modelProviderFilter,
        onboarding: this.host.providers.onboarding,
        themeBeforePreview: this.host.themes.previewBase?.name ?? null,
      },
    })
  }

  /** Rebuild view bindings from client-owned data; projection responses remain engine-owned. */
  restoreRecycleState(state: AppClientState): void {
    if (state.sessionId !== this.host.sessionId) return
    this.host.providers.suppressOnboarding()
    const theme = this.host.resolveTheme(themeByName(state.theme) ?? kennelTheme)
    if (theme.name !== this.host.theme.name) this.host.applyTheme(theme)
    this.restoreComposerState(state.composer)
    this.host.children.restoreDrafts({ content: state.composer.content, attachments: state.composer.attachments }, state.subagentDrafts)
    this.host.submission.restoreInput(state.composer.content)
    this.host.input.restore(state.inputMode, state.focus)
    this.host.setPrimaryView(state.primaryView)
    const picker = state.picker
    if (picker !== null) {
      switch (picker.kind) {
        case "palette": this.host.ui.openCommandPicker(); break
        case "keyboardHelp": this.host.ui.openKeyboardHelpPicker(); break
        case "commands": this.host.pickerContent.requestCommands(); break
        case "attachments": this.host.ui.openAttachmentPicker(); break
        case "mcp": this.host.ui.openMcpPicker(); break
        case "modes": this.host.ui.openModePicker(); break
        case "models": this.host.ui.openModelPicker(picker.modelProviderFilter); break
        case "providers": this.host.ui.openProviderPicker(picker.onboarding); break
        case "permissions": this.host.ui.openPermissionPicker(); break
        case "permissionMode": this.host.ui.openPermissionModePicker(); break
        case "trust": this.host.ui.openTrustPicker(); break
        case "queuedMessages": this.host.ui.openQueuedMessagesPicker(); break
        case "workspaceRoots": this.host.ui.openWorkspaceRootsPicker(); break
        case "budgets": this.host.ui.openBudgetPicker(); break
        case "sessions": this.host.ui.openSessionPicker(); break
        case "settings": this.host.ui.openSettingsPicker(); break
        case "agents": this.host.ui.openSubagentPicker(); break
        case "timeline": this.host.ui.openTimelinePicker(); break
        case "themes": this.host.ui.openThemePicker(); break
      }
      this.host.pickerController.begin(picker.kind, picker.anchored, picker.query)
      const surface = this.clientPickerSurface()
      if (surface !== null) surface.input.value = picker.query
      else this.host.ui.picker.input.value = picker.query
      this.host.themes.restorePreviewBase(picker.themeBeforePreview === null
        ? null : this.host.resolveTheme(themeByName(picker.themeBeforePreview) ?? kennelTheme))
      this.host.pickerController.refresh()
    }
    this.#pendingClientState = state
    this.host.ui.setState(this.host.ui.state)
    this.host.input.focusForInputMode()
  }

  /** Apply viewport/selection only after replay and OpenTUI layout have supplied their rows. */
  applyPendingRecycleScroll(): void {
    const state = this.#pendingClientState
    if (state === null) return
    const transcriptReady = state.scrollTop === 0 || this.host.ui.transcript.mountedEntryCount > 0
    if (state.tools.expanded.length > 0 || state.tools.selectedId !== null || state.toolsScrollTop > 0) {
      this.host.updateToolsWorkspace(this.host.children.presentedState(), true)
    }
    const toolsReady = state.toolsScrollTop === 0 || this.host.ui.toolsWorkspace.mountedRowCount > 0
    const transcriptBlocksReady = this.host.ui.transcript.restoreClientState(state.transcript)
    const toolsBlocksReady = this.host.ui.toolsWorkspace.restoreClientState(state.tools)
    if (transcriptReady) this.host.ui.transcript.setScrollOffset(state.scrollTop)
    if (toolsReady) this.host.ui.toolsWorkspace.activityScroller.scrollTo(state.toolsScrollTop)
    let pickerReady = true
    if (state.picker !== null && this.host.pickerController.kind === state.picker.kind) {
      const surface = this.clientPickerSurface()
      if (surface !== null) {
        if (state.picker.selectedId !== null) surface.selectById(state.picker.selectedId)
        pickerReady = state.picker.selectedId === null || surface.selectedId === state.picker.selectedId
        surface.restoreViewport(state.picker.scrollOffset)
      } else {
        const index = this.host.ui.picker.select.options.findIndex((item) => item.value === state.picker?.selectedId)
        if (index >= 0) this.host.ui.picker.select.setSelectedIndex(index)
        pickerReady = state.picker.selectedId === null || index >= 0
      }
    }
    if (transcriptReady && toolsReady && transcriptBlocksReady && toolsBlocksReady && pickerReady) this.#pendingClientState = null
  }

}
