import type { RecycleReview } from "../recycle-review"
import { interactionFingerprint, type InteractionSelection } from "../interaction-selection"
import type { ClientAllocationLease } from "../client-allocation"
import { retainedJsonBytes } from "../retained-json"
import { directSessionRead } from "../session-reader"
import type { RottweilerApp, PrimaryView } from "../app"
import type { PickerController } from "../picker-controller"
import {
  isRestorablePicker,
  parseTuiRecycleState,
  type AppClientState,
  type ClientComposerState,
} from "../recycle-state"
import type { HistoryController } from "../history/controller"
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
  readonly history: HistoryController
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
  readonly reviewRestorePending: boolean
  captureReview(): RecycleReview | null
  cancelReview(): void
  restoreReview(state: RecycleReview): void
  applyReview(): void
  resolveTheme(theme: RottweilerTheme): RottweilerTheme
  applyTheme(theme: RottweilerTheme): void
  setPrimaryView(view: PrimaryView): void
  updateToolsWorkspace(state: RottweilerState, restoreHidden: boolean): void
}
interface MountedClientState {
  transcript: AppClientState["transcript"] | null
  tools: AppClientState["tools"] | null
  toolsScrollTop: number
  picker: AppClientState["picker"]
}

export class ClientRestoreController {
  #pendingClientState: AppClientState | null = null
  #pendingAllocation: ClientAllocationLease | null = null
  #mountedState: MountedClientState | null = null
  #mountedAllocation: ClientAllocationLease | null = null
  #pendingMounted = false
  #transcriptAttempt: string | null = null
  #toolsAttempt: string | null = null
  #pickerAttempt: string | null = null
  #toolsState: WeakRef<RottweilerState> | null = null
  #interactionAllocation: ClientAllocationLease | null = null
  #answerGuard: { readonly session: string; readonly control: string; readonly text: string; readonly child: string } | null = null
  #interaction: InteractionSelection | null = null
  constructor(readonly host: ClientRestoreHost) {}
  #discardMountedState(): void {
    this.#pendingClientState = null; this.#mountedState = null
    this.#mountedAllocation?.release(); this.#mountedAllocation = null
    this.#transcriptAttempt = null; this.#toolsAttempt = null; this.#pickerAttempt = null
    this.#toolsState = null
    this.#pendingAllocation?.release(); this.#pendingAllocation = null
  }
  discard(): void {
    this.#discardMountedState(); this.#interaction = null
    this.#interactionAllocation?.release(); this.#interactionAllocation = null
  }
  dispose(): void { this.discard(); this.#answerGuard = null }
  admitAnswer(content: string): boolean {
    const guard = this.#answerGuard
    if (guard === null) return true
    if (guard.session !== this.host.sessionId) { this.#answerGuard = null; return true }
    const current = this.host.ui.interactionPanel.captureSelection()
    if (interactionFingerprint(content) !== guard.text || (current?.fingerprint === guard.control
      && interactionFingerprint(this.host.children.captureRecycleTarget()) === guard.child)) {
      this.#answerGuard = null
      return true
    }
    this.host.submission.notice = "This draft belongs to a different question. Edit it before sending."
    this.host.ui.setState(this.host.ui.state)
    return false
  }
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
    const review = this.host.reviewOpen ? this.host.captureReview() : null
    if (this.host.reviewRestorePending || this.host.submission.reviewPending || (this.host.reviewOpen && review === null)
      || this.host.children.controlsPending || (this.host.children.activeId !== null && this.host.children.captureRecycleTarget() === null) || this.host.children.draftStore.usage.pending > 0 || this.host.submissionsInFlight > 0
      || this.host.submission.terminalSuspended || this.host.ui.state.shell.active || this.host.ui.state.replay.active
      || this.host.providers.hasPendingAction
      || this.host.ui.state.providerAuth.pending !== null || this.host.mcp.hasDraft
      || this.host.sessions.pending
      || (kind === "timeline" && !this.host.sessions.timelineRestorable)
      || this.host.ui.outputViewer.visible
      || (kind !== null && !isRestorablePicker(kind))) return null
    const history = this.host.ui.transcript.captureHistoryViewport()
    if (history === null) return null
    const surface = this.clientPickerSurface()
    const selected = surface?.selectedId ?? this.host.ui.picker.select.getSelectedOption()?.value
    return parseTuiRecycleState({
      schemaVersion: 5, review,
      child: this.host.children.captureRecycleTarget(),
      parentComposer: this.host.children.activeId === null ? null : this.host.children.draftStore.get("parent"),
      interaction: this.host.ui.interactionPanel.captureSelection(),
      sessionId: this.host.sessionId,
      composer: this.captureComposerState(),
      subagentDrafts: this.host.children.activeId === null ? [...this.host.children.drafts] : [
        ...this.host.children.drafts.filter(entry => entry.id !== this.host.children.activeId),
        { id: this.host.children.activeId, draft: { content: this.host.ui.composer.value, attachments: [...this.host.ui.composer.attachments] } },
      ],
      primaryView: this.host.ui.primaryView,
      history,
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
  restoreRecycleState(state: AppClientState): boolean {
    if (state.sessionId !== this.host.sessionId) return false
    const owner = this.host.history.cache.allocations.reserve("drafts", retainedJsonBytes(state, 64 * 1024 * 1024))
    let installed = false
    try {
    if (!this.host.children.restoreDrafts(state.parentComposer ?? { content: state.composer.content, attachments: state.composer.attachments }, state.subagentDrafts)) { owner.release(); return false }
    this.discard(); this.#pendingMounted = state.child === null
    this.#answerGuard = state.interaction?.composer ? { session: state.sessionId, control: state.interaction.fingerprint,
      text: interactionFingerprint(state.composer.content), child: interactionFingerprint(state.child) } : null
    this.host.providers.suppressOnboarding()
    const theme = this.host.resolveTheme(themeByName(state.theme) ?? kennelTheme)
    if (theme.name !== this.host.theme.name) this.host.applyTheme(theme)
    if (state.child === null) this.restoreComposerState(state.composer)
    else this.host.children.restoreComposerDraft(null)
    this.host.submission.restoreInput(this.host.ui.composer.value)
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
        case "timeline": this.host.sessions.openTimelinePicker(picker.selectedId?.match(/^timeline\.turn\.([0-9]+)$/)?.[1]); break
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
    if (state.child === null) void this.host.history.restoreViewport(directSessionRead(this.host.sessionId), state.history)
    this.#pendingAllocation = owner
    const mounted: MountedClientState = { transcript: state.transcript, tools: state.tools,
      toolsScrollTop: state.toolsScrollTop, picker: state.picker }
    // Reserve every later restoration owner before the private handoff can be
    // consumed. Frame-time adoption must not need new allocation admission.
    this.#mountedAllocation = this.host.history.cache.allocations.reserve("drafts", retainedJsonBytes(mounted, 64 * 1024 * 1024))
    this.#mountedState = mounted
    if (state.interaction !== null) {
      this.#interactionAllocation = this.host.history.cache.allocations.reserve("drafts", retainedJsonBytes(state.interaction, 4096))
      this.#interaction = state.interaction
    }
    this.#pendingClientState = state
    if (state.review !== null) this.host.restoreReview(state.review)
    this.host.ui.setState(this.host.ui.state)
    this.host.input.focusForInputMode()
    installed = true
    return true
    } finally { if (!installed) { this.host.cancelReview(); if (this.#pendingAllocation === owner) this.discard(); owner.release() } }
  }

  /** Apply viewport/selection only after replay and OpenTUI layout have supplied their rows. */
  applyPendingRecycleScroll(): void {
    this.host.applyReview()
    const pending = this.#pendingClientState
    if (pending !== null) {
      if (!this.#pendingMounted) {
        if (pending.child === null || !this.host.children.restoreRecycleTarget(pending.child)) return
        this.#pendingMounted = true
        this.restoreComposerState(pending.composer); this.host.submission.restoreInput(pending.composer.content)
        void this.host.history.restoreViewport(this.host.children.readTarget, pending.history)
      }
      // Only separately admitted view hints await later mounts. Composer and
      // attachment owners have adopted their state and no longer need this envelope.
      this.#pendingClientState = null
      this.#pendingAllocation?.release(); this.#pendingAllocation = null
    }
    const state = this.#mountedState
    if (state === null) { this.#restoreInteraction(); return }
    this.#restoreInteraction()
    const transcriptReady = !this.host.history.snapshot.loading
      && this.host.history.snapshot.page !== null
    if (state.tools !== null && (state.tools.expanded.length > 0 || state.tools.selectedId !== null || state.toolsScrollTop > 0)) {
      const presented = this.host.children.presentedState()
      if (this.#toolsState?.deref() !== presented) {
        this.host.updateToolsWorkspace(presented, true)
        this.#toolsState = new WeakRef(presented)
      }
    }
    const toolsReady = state.toolsScrollTop === 0 || this.host.ui.toolsWorkspace.mountedRowCount > 0
    const transcript = this.host.ui.transcript, tools = this.host.ui.toolsWorkspace
    const transcriptRevision = `${transcript.clientStateRevision}:${transcript.width}:${transcript.height}`
    if (state.transcript !== null && transcriptReady && this.#transcriptAttempt !== transcriptRevision) {
      this.#transcriptAttempt = transcriptRevision
      if (transcript.restoreClientState(state.transcript)) this.#retireSurface("transcript")
    }
    const toolsRevision = `${tools.clientStateRevision}:${tools.width}:${tools.height}`
    if (state.tools !== null && toolsReady && this.#toolsAttempt !== toolsRevision) {
      this.#toolsAttempt = toolsRevision
      const restored = tools.restoreClientState(state.tools)
      tools.activityScroller.scrollTo(state.toolsScrollTop)
      if (restored) this.#retireSurface("tools")
    }
    let pickerReady = true
    if (state.picker !== null && this.host.pickerController.kind === state.picker.kind) {
      const surface = this.clientPickerSurface()
      if (surface !== null) {
        const revision = `${surface.clientStateRevision}:${surface.width}:${surface.height}`
        if (this.#pickerAttempt === revision) return
        this.#pickerAttempt = revision
        if (state.picker.selectedId !== null) surface.selectById(state.picker.selectedId)
        pickerReady = state.picker.selectedId === null || surface.selectedId === state.picker.selectedId
        surface.restoreViewport(state.picker.scrollOffset)
      } else {
        const options = this.host.ui.picker.select.options
        const revision = `${this.host.ui.picker.clientStateRevision}:${this.host.ui.picker.width}:${this.host.ui.picker.height}`
        if (this.#pickerAttempt === revision) return
        this.#pickerAttempt = revision
        const index = options.findIndex((item) => item.value === state.picker?.selectedId)
        if (index >= 0) this.host.ui.picker.select.setSelectedIndex(index)
        pickerReady = state.picker.selectedId === null || index >= 0
      }
    }
    if (pickerReady) this.#retireSurface("picker")
    if (state.transcript === null && state.tools === null && state.picker === null) this.#discardMountedState()
  }

  #retireSurface(surface: "transcript" | "tools" | "picker"): void {
    const state = this.#mountedState
    if (state === null || state[surface] === null) return
    state[surface] = null
    if (surface === "tools") state.toolsScrollTop = 0
    this.#mountedAllocation?.resize(retainedJsonBytes(state, 64 * 1024 * 1024))
  }

  #restoreInteraction(): void {
    if (this.#interaction === null || !this.host.ui.interactionPanel.restoreSelection(this.#interaction)) return
    this.#interaction = null
    this.#interactionAllocation?.release(); this.#interactionAllocation = null
  }
}
