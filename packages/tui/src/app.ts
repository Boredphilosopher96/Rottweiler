import {
  BoxRenderable,
  CliRenderEvents,
  type KeyEvent,
  type RenderContext,
  type TreeSitterClient,
} from "@opentui/core"

import {
  ComposerRenderable,
  ContextPanelRenderable,
  FuzzyPickerRenderable,
  InteractionPanelRenderable,
  ReviewPanelRenderable,
  StateBannerRenderable,
  StatusLineRenderable,
  TranscriptRenderable,
  type PickerItem,
} from "./components"
import {
  compileKeybindings,
  type CompiledKeybindings,
  type InputMode,
  type KeybindingAction,
  type KeybindingConfiguration,
  type KeybindingContext,
  type VimFocus,
} from "./keybindings"
import {
  noExternalEditor,
  noImagePaste,
  noNotifications,
  type EditorAdapter,
  type ImagePasteAdapter,
  type NotificationAdapter,
} from "./platform"
import {
  PROTOCOL_VERSION,
  type ApprovalDecision,
  type ApprovalBinding,
  type Attachment,
  type ClientCommand,
  type CommandOutcome,
  type EngineEvent,
  type PlanDecision,
  type ModeId,
} from "./protocol"
import {
  createInitialState,
  enterReplayMode,
  engineEvent,
  reduceRottweilerState,
  type QuestionProjection,
  type RottweilerState,
  type ToolProjection,
} from "./state"
import { createSyntaxStyle, kennelTheme, type RottweilerTheme } from "./theme"
import { isRecord, isSessionForkedEvent, type WireEngineEvent } from "./transport"

export interface RottweilerAppOptions {
  readonly initialEvent?: EngineEvent
  readonly initialState?: RottweilerState
  readonly sessionId?: string
  readonly clientId?: string
  readonly onCommand?: (
    command: ClientCommand,
  ) => void | CommandOutcome | null | Promise<void | CommandOutcome | null>
  readonly requestId?: () => string
  readonly theme?: RottweilerTheme
  readonly treeSitterClient?: TreeSitterClient
  readonly notifications?: NotificationAdapter
  readonly editor?: EditorAdapter
  readonly imagePaste?: ImagePasteAdapter
  readonly terminalHandover?: TerminalHandoverAdapter
  readonly onSessionSelect?: (sessionId: string) => void | Promise<void>
  /** Historical presentation is observer-only; the composer and mutating interactions are hidden. */
  readonly replaySessionId?: string
  /** TUI-local bindings. Standard is backward-compatible; Vim enables modal editing/navigation. */
  readonly keybindings?: KeybindingConfiguration
}

export interface TerminalHandoverAdapter {
  suspend(): void
  resume(): void
}

type PickerKind = "palette" | "commands" | "files" | "modes" | "models" | "providers" | "providerAuth" | "sessions" | "settings"
type ProjectionKind = "commands" | "models" | "sessions" | "files"
const MAX_PENDING_MODEL_SWITCH_REQUESTS = 128

type CommandChoice = RottweilerState["commands"][number]
type ModelPickerChoice =
  | { readonly kind: "alias"; readonly alias: RottweilerState["modelAliases"][number] }
  | { readonly kind: "model"; readonly model: RottweilerState["models"][number] }

const LOCAL_SLASH_COMMANDS: readonly CommandChoice[] = [
  { name: "help", description: "List available commands", usage: "/help" },
  { name: "status", description: "Show actor running and queue state", usage: "/status" },
  { name: "mode", description: "Show or switch the interaction mode", usage: "/mode [discuss|plan|execute]" },
  { name: "models", description: "Switch the active model", usage: "/models" },
  { name: "providers", description: "Choose a configured provider and model", usage: "/providers" },
  { name: "settings", description: "Change safe user settings", usage: "/settings" },
  { name: "permissions", description: "Show or edit session permission rules", usage: "/permissions [list|approvals|add|remove|clear-session|revoke-session|revoke-project]" },
  { name: "plan", description: "Show the pending or approved plan", usage: "/plan" },
  { name: "rewind", description: "Restore a completed turn checkpoint", usage: "/rewind <turn>" },
  { name: "fork", description: "Fork this session at a completed turn", usage: "/fork [turn]" },
  { name: "review", description: "Review the cumulative session diff", usage: "/review" },
  { name: "interrupt", description: "Interrupt the active turn", usage: "/interrupt" },
  { name: "context", description: "Inspect, pin, or evict context items", usage: "/context [pin|evict <item-id>]" },
  { name: "cost", description: "Show usage, cost, and budget accounting", usage: "/cost" },
  { name: "compact", description: "Compact conversation context", usage: "/compact [instructions]" },
  { name: "trust", description: "Inspect or change folder trust", usage: "/trust [status|grant|revoke]" },
  { name: "add-dir", description: "Append a live workspace root", usage: "/add-dir <path>" },
]

interface PaletteAction {
  readonly id: string
  readonly title: string
  readonly description: string
  readonly category: string
  readonly run: () => void
}

export class RottweilerApp extends BoxRenderable {
  readonly transcript: TranscriptRenderable
  readonly contextPanel: ContextPanelRenderable
  readonly interactionPanel: InteractionPanelRenderable
  readonly reviewPanel: ReviewPanelRenderable
  readonly picker: FuzzyPickerRenderable<unknown>
  readonly composer: ComposerRenderable
  readonly statusLine: StatusLineRenderable
  readonly banner: StateBannerRenderable
  readonly main: BoxRenderable

  #state: RottweilerState
  #options: Required<
    Pick<RottweilerAppOptions, "sessionId" | "clientId" | "requestId" | "notifications" | "editor" | "imagePaste">
  > &
    RottweilerAppOptions
  #syntaxStyle: ReturnType<typeof createSyntaxStyle>
  #sessionId: string
  #terminalFocused = true
  #pickerKind: PickerKind | null = null
  #pendingFilePreview: { readonly path: string; readonly requestId: string } | null = null
  #pendingWorkspaceSearchRequest: string | null = null
  #latestWorkspaceStatusRequest: string | null = null
  #latestWorkspaceDiffRequest: string | null = null
  #pendingWorkspaceDiffPath: string | null = null
  #latestReviewRequest: string | null = null
  #latestCommandsRequest: string | null = null
  #latestModelsRequest: string | null = null
  #latestSessionsRequest: string | null = null
  #commandsRequested = false
  #modelsRequested = false
  #projectionErrors: Partial<Record<ProjectionKind, string>> = {}
  #pickerAnchored = false
  #pickerQuery = ""
  #modelProviderFilter: string | null = null
  #reviewOpen = false
  #pendingReviewSelection: string | null = null
  #postSubmitPicker: "models" | "providers" | "settings" | null = null
  #terminalSuspended = false
  #pendingShellTimer: ReturnType<typeof setTimeout> | null = null
  #pluginNotificationTimer: ReturnType<typeof setTimeout> | null = null
  #sessionSearchTimer: ReturnType<typeof setTimeout> | null = null
  #pendingForkRequests = new Set<string>()
  #pendingReviewPaths = new Set<string>()
  #pendingModelSwitchRequests = new Set<string>()
  #pendingModelSelections = new Map<string, string>()
  #keybindings: CompiledKeybindings
  #inputMode: InputMode
  #vimFocus: VimFocus = "composer"
  #vimFocusBeforePicker: Exclude<VimFocus, "picker"> = "composer"
  #onTerminalFocus = () => {
    this.#terminalFocused = true
  }
  #onTerminalBlur = () => {
    this.#terminalFocused = false
  }
  #onGlobalKey = (key: KeyEvent) => {
    const focusOwner = this.#visibleFocusOwner()
    const safetyPanelFocused = focusOwner === "interaction" || focusOwner === "review"
    const action =
      focusOwner === "review"
        ? this.#keybindings.resolve("review", key)
        : focusOwner === "interaction"
          ? null
          : this.#keybindings.resolve("global", key) ??
            this.#keybindings.resolve(this.#keybindingContext(), key)
    if (action !== null && this.#handleKeybindingAction(action)) {
      key.preventDefault()
      key.stopPropagation()
    } else if (
      this.#keybindings.preset === "vim" &&
      this.#inputMode === "normal" &&
      !safetyPanelFocused &&
      !key.ctrl &&
      !key.meta &&
      !key.option
    ) {
      // A focused OpenTUI editor still owns the terminal cursor in normal mode.
      // Never let an unmapped printable/navigation key leak through as text.
      key.preventDefault()
      key.stopPropagation()
    }
  }

  constructor(ctx: RenderContext, options: RottweilerAppOptions = {}) {
    const theme = options.theme ?? kennelTheme
    super(ctx, {
      id: "rottweiler-app",
      width: "100%",
      height: "100%",
      flexDirection: "column",
      backgroundColor: theme.background,
    })
    this.#options = {
      ...options,
      sessionId: options.sessionId ?? "session-local",
      clientId: options.clientId ?? "tui-client",
      requestId: options.requestId ?? (() => crypto.randomUUID()),
      notifications: options.notifications ?? noNotifications,
      editor: options.editor ?? noExternalEditor,
      imagePaste: options.imagePaste ?? noImagePaste,
    }
    this.#keybindings = compileKeybindings(options.keybindings)
    this.#inputMode = this.#keybindings.preset === "vim" ? "normal" : "standard"
    this.#syntaxStyle = createSyntaxStyle(theme)
    this.#sessionId = this.#options.sessionId
    const initialState = options.initialState ?? createInitialState()
    this.#state =
      options.replaySessionId === undefined
        ? initialState
        : enterReplayMode(initialState, options.replaySessionId)
    if (this.#state.replay.active && this.#keybindings.preset === "vim") {
      this.#vimFocus = "transcript"
      this.#vimFocusBeforePicker = "transcript"
    }
    if (options.initialEvent !== undefined) {
      this.#state = reduceRottweilerState(this.#state, engineEvent(options.initialEvent))
    }

    this.banner = new StateBannerRenderable(ctx, theme)
    this.main = new BoxRenderable(ctx, {
      id: "main-content",
      width: "100%",
      flexGrow: 1,
      minHeight: 1,
      flexDirection: "row",
      backgroundColor: theme.background,
      gap: 1,
    })
    this.transcript = new TranscriptRenderable(ctx, theme, {
      syntaxStyle: this.#syntaxStyle,
      ...(options.treeSitterClient === undefined
        ? {}
        : { treeSitterClient: options.treeSitterClient }),
      overscan: 3,
    })
    this.contextPanel = new ContextPanelRenderable(ctx, theme, {
      onOpenDiff: (path) => this.#openChangedFileDiff(path),
    })
    this.main.add(this.transcript)
    this.main.add(this.contextPanel)

    this.interactionPanel = new InteractionPanelRenderable(
      ctx,
      theme,
      this.#syntaxStyle,
      {
        onApproval: (tool, decision) => this.#approve(tool, decision),
        onAnswer: (question, values) => this.#answer(question, values),
        onPlanReview: (decision) => this.#reviewPlan(decision),
      },
      options.treeSitterClient,
    )
    this.reviewPanel = new ReviewPanelRenderable(
      ctx,
      theme,
      this.#syntaxStyle,
      {
        onDecision: (file, decision) =>
          void this.#reviewFile(file.path, file.currentHash, decision),
        onClose: () => this.#closeReview(),
      },
      options.treeSitterClient,
    )
    this.picker = new FuzzyPickerRenderable(ctx, theme, (query) => {
      if (this.#pickerKind === "sessions") {
        this.#scheduleSessionSearch(query)
      }
    })
    this.picker.position = "absolute"
    this.picker.top = 2
    this.picker.left = "15%"
    this.picker.width = "70%"
    this.composer = new ComposerRenderable(ctx, theme, {
      editor: this.#options.editor,
      imagePaste: this.#options.imagePaste,
      onSubmit: (content, attachments) => this.#sendMessage(content, attachments),
      onFileMention: (query) => this.openFilePicker(query, true),
      onInput: (value) => this.#updateComposerAutocomplete(value),
      onSubmitted: () => {
        const picker = this.#postSubmitPicker
        this.#postSubmitPicker = null
        if (picker === "models") this.openModelPicker()
        else if (picker === "providers") this.openProviderPicker()
        else if (picker === "settings") this.openSettingsPicker()
      },
    })
    this.statusLine = new StatusLineRenderable(ctx, theme)

    this.add(this.banner)
    this.add(this.main)
    this.add(this.reviewPanel)
    this.add(this.interactionPanel)
    this.add(this.composer)
    this.add(this.statusLine)
    this.add(this.picker)
    ctx.on(CliRenderEvents.FOCUS, this.#onTerminalFocus)
    ctx.on(CliRenderEvents.BLUR, this.#onTerminalBlur)
    ctx.keyInput.on("keypress", this.#onGlobalKey)
    this.setState(this.#state)
    if (this.#reviewOpen) {
      this.reviewPanel.files.focus()
    } else if (!this.#state.replay.active) {
      this.#focusForInputMode()
    }
  }

  get state(): RottweilerState {
    return this.#state
  }

  /** Update command routing only after the runtime owns the new driver lease. */
  setSessionId(sessionId: string): void {
    if (sessionId !== this.#sessionId) {
      this.#latestWorkspaceStatusRequest = null
      this.#latestReviewRequest = null
      this.#latestCommandsRequest = null
      this.#latestModelsRequest = null
      this.#latestSessionsRequest = null
      this.#commandsRequested = false
      this.#modelsRequested = false
      this.#projectionErrors = {}
      this.#pendingReviewSelection = null
      this.#reviewOpen = false
      this.#pendingModelSwitchRequests.clear()
      this.#pendingModelSelections.clear()
      this.reviewPanel.closePresentation()
    }
    this.#sessionId = sessionId
    if (this.#state.replay.active && this.#state.replay.sessionId !== sessionId) {
      this.setState(enterReplayMode(createInitialState(), sessionId))
    }
  }

  handleEvent(event: WireEngineEvent): void {
    const eventRecord = event as unknown as Record<string, unknown>
    const commandRequestId =
      isRecord(eventRecord.meta) && typeof eventRecord.meta.request_id === "string"
        ? eventRecord.meta.request_id
        : null
    if (event.type === "session_forked") {
      if (
        !isSessionForkedEvent(event) ||
        event.parent_session_id !== this.#sessionId ||
        !this.#pendingForkRequests.has(event.meta.request_id)
      ) return
      this.#pendingForkRequests.clear()
    }
    if (
      event.type === "sessions_search_ready" &&
      this.#pickerKind === "sessions" &&
      event.query !== this.picker.input.value
    ) {
      return
    }
    if (
      event.type === "workspace_status_ready" &&
      this.#latestWorkspaceStatusRequest !== null &&
      commandRequestId !== this.#latestWorkspaceStatusRequest
    ) {
      return
    }
    if (event.type === "workspace_diff_ready") {
      const diffPath =
        isRecord(eventRecord.diff) && typeof eventRecord.diff.path === "string"
          ? eventRecord.diff.path
          : null
      if (
        this.#latestWorkspaceDiffRequest === null ||
        commandRequestId !== this.#latestWorkspaceDiffRequest ||
        diffPath !== this.#pendingWorkspaceDiffPath
      ) return
    }
    if (
      event.type === "session_review_ready" &&
      this.#latestReviewRequest !== null &&
      commandRequestId !== this.#latestReviewRequest
    ) {
      return
    }
    if (event.type === "workspace_files_found" && this.#pendingWorkspaceSearchRequest !== null) {
      const requestId =
        isRecord(event.meta) && typeof event.meta.request_id === "string"
          ? event.meta.request_id
          : null
      if (requestId !== this.#pendingWorkspaceSearchRequest) return
    }
    if (event.type === "workspace_file_preview_ready") {
      const requestId =
        isRecord(event.meta) && typeof event.meta.request_id === "string"
          ? event.meta.request_id
          : null
      const path =
        isRecord(event.preview) && typeof event.preview.path === "string"
          ? event.preview.path
          : null
      if (
        this.#pendingFilePreview === null ||
        requestId !== this.#pendingFilePreview.requestId ||
        path !== this.#pendingFilePreview.path
      ) return
    }
    if (
      event.type === "command_descriptors_listed" &&
      this.#latestCommandsRequest !== null &&
      commandRequestId !== this.#latestCommandsRequest
    ) return
    if (
      event.type === "models_listed" &&
      this.#latestModelsRequest !== null &&
      commandRequestId !== this.#latestModelsRequest
    ) return
    if (
      (event.type === "sessions_listed" || event.type === "sessions_search_ready") &&
      this.#latestSessionsRequest !== null &&
      commandRequestId !== this.#latestSessionsRequest
    ) return
    if (event.type === "command_descriptors_listed") {
      this.#commandsRequested = false
      this.#latestCommandsRequest = null
      this.#clearProjectionError("commands")
    }
    if (event.type === "models_listed") {
      this.#modelsRequested = false
      this.#latestModelsRequest = null
      this.#clearProjectionError("models")
    }
    if (event.type === "sessions_listed" || event.type === "sessions_search_ready") {
      this.#clearProjectionError("sessions")
      this.#latestSessionsRequest = null
    }
    if (event.type === "workspace_files_found") this.#clearProjectionError("files")
    const previous = this.#state
    const next = reduceRottweilerState(previous, engineEvent(event))
    this.setState(next)
    const modelSwitchOutcome =
      event.type === "command_acknowledged" &&
      commandRequestId !== null &&
      this.#pendingModelSwitchRequests.delete(commandRequestId)
        ? next.commandAcks[commandRequestId]?.outcome
        : null
    if (modelSwitchOutcome?.type === "rejected") {
      if (commandRequestId !== null) this.#pendingModelSelections.delete(commandRequestId)
      this.#projectRejection(modelSwitchOutcome)
    }
    this.#notify(previous, next)
    if (isSessionForkedEvent(event)) {
      void this.#transitionToFork(event.child.session_id)
    }
    if (next.pluginNotifications.at(-1) !== previous.pluginNotifications.at(-1)) {
      this.#schedulePluginNotificationDismissal(next.pluginNotifications.at(-1))
    }
    if (event.type === "user_shell_state_changed") {
      if (event.active) {
        this.#clearPendingShellTimer()
        this.#suspendTerminal()
      } else {
        this.#resumeTerminal()
      }
    }
    if (
      next.workspacePreview !== previous.workspacePreview &&
      next.workspacePreview !== null &&
      this.#pendingFilePreview !== null
    ) {
      const preview = next.workspacePreview
      this.composer.addAttachment({
        name: preview.path,
        media_type: preview.mediaType,
        data: preview.data,
      })
      this.#pendingFilePreview = null
      this.closePicker()
    }
    if (event.type === "session_review_ready" || event.type === "session_review_updated") {
      const path = this.#pendingReviewSelection
      if (path !== null) {
        this.#pendingReviewSelection = null
        if (!this.reviewPanel.selectPath(path)) {
          this.#projectClientError(
            "diff_unavailable",
            `no retained session diff is available for ${path}`,
          )
          this.reviewPanel.showDiffMessage(path, "No retained session diff is available")
        }
      }
    }
    if (event.type === "workspace_diff_ready" && next.workspaceDiff !== null) {
      this.#pendingWorkspaceDiffPath = null
      this.reviewPanel.showWorkspaceDiff(
        next.workspaceDiff.path,
        next.workspaceDiff.unifiedDiff,
        next.workspaceDiff.binary,
        next.workspaceDiff.truncated,
      )
    }
    if (event.type === "command_finished" && event.name === "add-dir" && !next.replay.active) {
      this.#requestCommands()
    }
    if (event.type === "provider_auth_started") {
      const provider = typeof eventRecord.provider === "string" ? eventRecord.provider : null
      const attemptId = typeof eventRecord.attempt_id === "string" ? eventRecord.attempt_id : null
      if (provider === null || attemptId === null) return
      this.#command({
        type: "complete_provider_auth",
        provider,
        attemptId,
      })
      this.openProviderAuthPicker()
    }
    if (event.type === "model_changed" && isRecord(eventRecord.meta)) {
      const causedBy =
        typeof eventRecord.meta.caused_by === "string" ? eventRecord.meta.caused_by : null
      const concrete = causedBy === null ? undefined : this.#pendingModelSelections.get(causedBy)
      if (causedBy !== null) this.#pendingModelSelections.delete(causedBy)
      if (concrete !== undefined) {
        this.#command({ type: "set_setting", key: "project.models.default", value: concrete })
      }
    }
    if (event.type === "provider_configured") {
      const provider = typeof eventRecord.provider === "string" ? eventRecord.provider : null
      if (provider === null) return
      if (eventRecord.auth_kind === "oauth" || eventRecord.auth_kind === "device_flow") {
        this.#command({ type: "begin_provider_auth", provider })
      } else if (eventRecord.auth_kind === "api_key") {
        this.#projectClientError(
          "provider_api_key_cli_required",
          `Provider profile created. API keys never enter the replayable UI protocol; run rw auth set-key ${provider}`,
          true,
        )
      }
    }
    if (event.type === "provider_auth_finished") {
      if (eventRecord.success === true) {
        this.#requestModels(true)
        this.openProviderPicker()
      } else {
        this.#projectClientError(
          "provider_auth_failed",
          typeof eventRecord.message === "string" ? eventRecord.message : "provider authentication failed",
          true,
        )
      }
    }
    if (
      event.type === "tool_call_finished" ||
      event.type === "conversation_rewound" ||
      event.type === "session_review_updated" ||
      event.type === "command_finished" ||
      (event.type === "user_shell_state_changed" && !event.active)
    ) {
      this.#command({ type: "get_workspace_status" })
    }
  }

  setState(state: RottweilerState): void {
    const previousFocusOwner = this.#visibleFocusOwner()
    this.#state = state
    this.transcript.update(state)
    this.contextPanel.update(state)
    this.contextPanel.visible =
      !state.replay.active && (this.width === 0 ? this.ctx.width >= 100 : this.width >= 100)
    this.interactionPanel.update(state)
    this.reviewPanel.update(state, this.#reviewOpen)
    this.composer.setQueuedMessages(state.queuedMessages)
    this.composer.visible = !state.replay.active && !this.#reviewOpen
    const focusOwner = this.#visibleFocusOwner()
    if (
      (previousFocusOwner === "interaction" || previousFocusOwner === "review") &&
      focusOwner !== "interaction" &&
      focusOwner !== "review"
    ) {
      this.#focusForInputMode()
    }
    this.statusLine.setBranch(state.workspaceStatus?.branch ?? null)
    this.statusLine.setKeybindingMode(
      this.#inputMode === "standard" ? null : this.#inputMode,
      this.#inputMode === "standard" ? null : focusOwner,
    )
    this.statusLine.update(state)
    this.banner.update(state)
    if (this.#pickerKind !== null) {
      this.#refreshPicker()
    }
  }

  openCommandPicker(): void {
    this.#pickerAnchored = false
    this.#pickerQuery = ""
    this.#positionPicker(false)
    this.#pickerKind = "palette"
    if (!this.#commandsRequested) {
      this.#requestCommands()
    }
    this.#refreshPicker()
  }

  openFilePicker(query = "", anchored = false): void {
    this.#pickerAnchored = anchored
    this.#pickerQuery = query
    this.#positionPicker(anchored)
    this.#pickerKind = "files"
    this.#pendingWorkspaceSearchRequest =
      this.#command({ type: "search_workspace_files", query, limit: 100 }) ?? null
    this.#refreshPicker()
    if (!anchored) this.picker.input.value = query
  }

  openModelPicker(provider: string | null = null): void {
    this.#pickerAnchored = false
    this.#pickerQuery = ""
    this.#modelProviderFilter = provider
    this.#positionPicker(false)
    this.#pickerKind = "models"
    if (!this.#modelsRequested) {
      this.#requestModels()
    }
    this.#refreshPicker()
  }

  openProviderPicker(): void {
    this.#pickerAnchored = false
    this.#pickerQuery = ""
    this.#modelProviderFilter = null
    this.#positionPicker(false)
    this.#pickerKind = "providers"
    if (!this.#modelsRequested) {
      this.#requestModels()
    }
    this.#refreshPicker()
  }

  openProviderAuthPicker(): void {
    this.#pickerAnchored = false
    this.#pickerQuery = ""
    this.#positionPicker(false)
    this.#pickerKind = "providerAuth"
    this.#refreshPicker()
  }

  openSettingsPicker(): void {
    this.#pickerAnchored = false
    this.#pickerQuery = ""
    this.#positionPicker(false)
    this.#pickerKind = "settings"
    this.#command({ type: "list_settings" })
    this.#refreshPicker()
  }

  openModePicker(): void {
    this.#pickerAnchored = false
    this.#pickerQuery = ""
    this.#positionPicker(false)
    this.#pickerKind = "modes"
    this.#refreshPicker()
  }

  openSessionPicker(): void {
    this.#pickerAnchored = false
    this.#pickerQuery = ""
    this.#positionPicker(false)
    this.#pickerKind = "sessions"
    this.#command({ type: "list_sessions" })
    this.#refreshPicker()
  }

  openReview(): void {
    if (this.#state.replay.active) return
    if (this.#state.shell.active) {
      this.#projectClientError(
        "review_unavailable_during_shell",
        "exit the foreground shell before opening session review",
      )
      return
    }
    this.reviewPanel.showSessionReview()
    this.#reviewOpen = true
    this.setState(this.#state)
    this.#command({ type: "get_session_review" })
  }

  closePicker(): void {
    this.#pickerKind = null
    this.picker.close()
    this.#pickerAnchored = false
    this.#pickerQuery = ""
    this.#pendingWorkspaceSearchRequest = null
    this.#pendingFilePreview = null
    if (this.#keybindings.preset === "vim") this.#vimFocus = this.#vimFocusBeforePicker
    if (!this.#state.replay.active) this.#focusForInputMode()
    if (this.#keybindings.preset === "vim") {
      this.statusLine.setKeybindingMode(
        this.#inputMode === "normal" ? "normal" : "insert",
        this.#visibleFocusOwner(),
      )
      this.statusLine.update(this.#state)
    }
  }

  protected override onResize(width: number, height: number): void {
    this.contextPanel.visible = !this.#state.replay.active && width >= 100
    this.composer.resizeForTerminal(height)
    this.reviewPanel.resizeForTerminal(height)
    if (this.#pickerAnchored) this.#positionPicker(true)
  }

  override destroy(): void {
    this.#clearPendingShellTimer()
    this.#clearPluginNotificationTimer()
    this.#clearSessionSearchTimer()
    this.ctx.off(CliRenderEvents.FOCUS, this.#onTerminalFocus)
    this.ctx.off(CliRenderEvents.BLUR, this.#onTerminalBlur)
    this.ctx.keyInput.off("keypress", this.#onGlobalKey)
    this.#syntaxStyle.destroy()
    super.destroy()
  }

  #keybindingContext(): KeybindingContext {
    if (this.#keybindings.preset === "standard") {
      return this.#reviewOpen ? "review" : "standard"
    }
    if (this.picker.visible && !this.#pickerAnchored) {
      return this.#inputMode === "insert" ? "picker_insert" : "picker_normal"
    }
    if (this.#reviewOpen) return "review"
    return this.#inputMode === "insert" ? "vim_insert" : "vim_normal"
  }

  #handleKeybindingAction(action: KeybindingAction): boolean {
    if (action === "close_overlay") {
      if (this.picker.visible) {
        this.closePicker()
        return true
      }
      if (this.#reviewOpen) {
        this.#closeReview()
        return true
      }
      return false
    }
    if (action === "open_session_picker") {
      this.openSessionPicker()
      return true
    }
    if (this.#state.replay.active) {
      return this.#handleReplayNavigation(action)
    }
    switch (action) {
      case "cycle_agent_mode": {
        const current = this.#state.mode ?? "execute"
        const mode: ModeId =
          current === "execute" ? "discuss" : current === "discuss" ? "plan" : "execute"
        this.#emit({
          type: "switch_mode",
          meta: this.#meta(),
          session_id: this.#sessionId,
          mode,
        })
        return true
      }
      case "open_review":
        this.openReview()
        return true
      case "open_command_picker":
        this.openCommandPicker()
        return true
      case "open_model_picker":
        this.openModelPicker()
        return true
      case "open_mode_picker":
        this.openModePicker()
        return true
      case "paste_image":
        void this.composer.pasteImage()
        return true
      case "open_external_editor":
        if (this.picker.visible || this.#reviewOpen) return false
        void this.composer.openExternalEditor()
        return true
      case "enter_normal":
        this.#setInputMode("normal")
        return true
      case "enter_insert":
        this.#vimFocus = this.picker.visible ? "picker" : "composer"
        this.#setInputMode("insert")
        return true
      case "append_insert":
        if (!this.picker.visible && this.#vimFocus === "composer") {
          this.composer.editor.moveCursorRight()
        }
        this.#vimFocus = this.picker.visible ? "picker" : "composer"
        this.#setInputMode("insert")
        return true
      case "focus_next":
        this.#cycleVimFocus(1)
        return true
      case "focus_previous":
        this.#cycleVimFocus(-1)
        return true
      case "move_left":
        if (this.#vimFocus === "composer") this.composer.editor.moveCursorLeft()
        return true
      case "move_right":
        if (this.#vimFocus === "composer") this.composer.editor.moveCursorRight()
        return true
      case "move_up":
        this.#moveVertical(-1)
        return true
      case "move_down":
        this.#moveVertical(1)
        return true
      case "word_backward":
        if (this.#vimFocus === "composer") this.composer.editor.moveWordBackward()
        return true
      case "word_forward":
        if (this.#vimFocus === "composer") this.composer.editor.moveWordForward()
        return true
      case "line_start":
        if (this.#vimFocus === "composer") this.composer.editor.gotoLineStart()
        return true
      case "line_end":
        if (this.#vimFocus === "composer") this.composer.editor.gotoLineTextEnd()
        return true
      case "delete_character":
        if (this.#vimFocus === "composer") this.composer.editor.deleteChar()
        return true
      case "page_up":
        this.#scrollTranscript(-1, "viewport")
        return true
      case "page_down":
        this.#scrollTranscript(1, "viewport")
        return true
      case "view_top":
        this.#moveToBoundary(false)
        return true
      case "view_bottom":
        this.#moveToBoundary(true)
        return true
      case "select_current":
        if (!this.picker.visible) return false
        this.picker.select.selectCurrent()
        return true
    }
  }

  #handleReplayNavigation(action: KeybindingAction): boolean {
    if (this.#keybindings.preset !== "vim") return false
    switch (action) {
      case "move_up":
        this.#scrollTranscript(-1, "step")
        return true
      case "move_down":
        this.#scrollTranscript(1, "step")
        return true
      case "page_up":
        this.#scrollTranscript(-1, "viewport")
        return true
      case "page_down":
        this.#scrollTranscript(1, "viewport")
        return true
      case "view_top":
        this.transcript.scroller.scrollTo(0)
        return true
      case "view_bottom":
        this.transcript.scroller.scrollTo(this.transcript.scroller.scrollHeight)
        return true
      default:
        return false
    }
  }

  #setInputMode(mode: Exclude<InputMode, "standard">): void {
    if (this.#keybindings.preset !== "vim") return
    this.#inputMode = mode
    this.#focusForInputMode()
    this.statusLine.setKeybindingMode(mode, this.#visibleFocusOwner())
    this.statusLine.update(this.#state)
  }

  #focusForInputMode(): void {
    if (this.reviewPanel.visible) {
      this.reviewPanel.focusPresentation()
      return
    }
    if (this.interactionPanel.visible) {
      this.interactionPanel.select.focus()
      return
    }
    if (this.#inputMode === "standard") {
      this.composer.editor.showCursor = true
      this.composer.focus()
      return
    }
    if (this.picker.visible && !this.#pickerAnchored) {
      if (this.#inputMode === "insert") {
        this.picker.input.focus()
      } else {
        this.picker.select.focus()
      }
      return
    }
    this.composer.editor.showCursor = this.#inputMode === "insert"
    if (this.#vimFocus === "transcript" || this.#state.replay.active) {
      this.transcript.scroller.focus()
    } else {
      this.composer.focus()
    }
  }

  #cycleVimFocus(direction: 1 | -1): void {
    if (this.#keybindings.preset !== "vim" || this.picker.visible) return
    const targets: readonly Exclude<VimFocus, "picker">[] = ["composer", "transcript"]
    const current = Math.max(0, targets.indexOf(this.#vimFocus as Exclude<VimFocus, "picker">))
    this.#vimFocus = targets[(current + direction + targets.length) % targets.length] ?? "composer"
    this.#focusForInputMode()
    this.statusLine.setKeybindingMode("normal", this.#vimFocus)
    this.statusLine.update(this.#state)
  }

  #moveVertical(direction: 1 | -1): void {
    if (this.picker.visible) {
      this.picker.moveSelection(direction)
    } else if (this.#vimFocus === "composer") {
      if (direction < 0) this.composer.editor.moveCursorUp()
      else this.composer.editor.moveCursorDown()
    } else {
      this.#scrollTranscript(direction, "step")
    }
  }

  #scrollTranscript(direction: 1 | -1, unit: "step" | "viewport"): void {
    this.transcript.scroller.scrollBy(direction, unit)
  }

  #moveToBoundary(end: boolean): void {
    if (this.picker.visible) {
      this.picker.moveToBoundary(end)
    } else if (this.#vimFocus === "composer") {
      if (end) this.composer.editor.gotoBufferEnd()
      else this.composer.editor.gotoBufferHome()
    } else {
      this.transcript.scroller.scrollTo(end ? this.transcript.scroller.scrollHeight : 0)
    }
  }

  #visibleFocusOwner(): VimFocus | "interaction" | "review" {
    if (this.picker.visible && !this.#pickerAnchored) return "picker"
    if (this.reviewPanel.visible) return "review"
    if (this.interactionPanel.visible) return "interaction"
    if (this.#state.replay.active) return "transcript"
    return this.#vimFocus
  }

  #refreshPicker(): void {
    switch (this.#pickerKind) {
      case "palette":
        this.#openPicker(
          "Command palette",
          this.#paletteActions().map((action) => ({
            id: action.id,
            label: action.title,
            description: `${action.category} · ${action.description}`,
            searchText: `${action.category} ${action.title} ${action.description}`,
            value: action,
          })),
          (item) => (item.value as PaletteAction).run(),
        )
        break
      case "commands":
        const commandError = this.#projectionErrors.commands
        const commandItems: PickerItem<CommandChoice | null>[] = [
          ...(commandError === undefined
            ? []
            : [{
                id: "commands.error",
                label: "Couldn't load live commands",
                description: `${commandError} · select to retry`,
                value: null,
              }]),
          ...this.#slashCommandChoices().map((command) => ({
            id: command.name,
            label: `/${command.name}`,
            description: `${commandSourceLabel(command.source)} · ${command.description}`,
            searchText: command.usage,
            value: command,
          })),
        ]
        this.#openPicker(
          this.#state.commandsTruncated ? "Commands · results truncated" : "Commands",
          commandItems,
          (item) => {
            const command = item.value as CommandChoice | null
            if (command === null) {
              this.#requestCommands()
              return
            }
            const clearAnchoredTrigger = () => {
              if (this.#pickerAnchored) this.composer.value = ""
            }
            if (command.name === "review") {
              clearAnchoredTrigger()
              this.openReview()
              this.closePicker()
              return
            }
            if (command.name === "fork") {
              clearAnchoredTrigger()
              void this.#requestFork(null)
              this.closePicker()
              return
            }
            if (command.name === "models") {
              clearAnchoredTrigger()
              this.closePicker()
              this.openModelPicker()
              return
            }
            if (command.name === "providers") {
              clearAnchoredTrigger()
              this.closePicker()
              this.openProviderPicker()
              return
            }
            if (command.name === "settings") {
              clearAnchoredTrigger()
              this.closePicker()
              this.openSettingsPicker()
              return
            }
            this.composer.value = `/${command.name} `
            this.closePicker()
          },
        )
        break
      case "files":
        const fileError = this.#projectionErrors.files
        const fileItems: PickerItem<RottweilerState["workspaceFiles"][number] | null>[] = [
          ...(fileError === undefined
            ? []
            : [{
                id: "files.error",
                label: "Couldn't search workspace files",
                description: `${fileError} · select to retry`,
                value: null,
              }]),
          ...this.#state.workspaceFiles.map((file) => ({
            id: file.path,
            label: file.isDirectory ? `▸ ${file.path}` : file.path,
            description: file.isDirectory ? "directory" : "attach file",
            value: file,
          })),
        ]
        this.#openPicker(
          "Workspace files",
          fileItems,
          (item) => {
            const file = item.value as RottweilerState["workspaceFiles"][number] | null
            if (file === null) {
              this.openFilePicker(this.#pickerQuery, this.#pickerAnchored)
              return
            }
            if (file.isDirectory) {
              const query = `${file.path.replace(/\/$/, "")}/`
              if (this.#pickerAnchored) {
                this.composer.value = this.composer.value.replace(
                  /@[^\s]*$/,
                  `@${query}`,
                )
              } else {
                this.openFilePicker(query)
              }
              return
            }
            const requestId = this.#command({
              type: "preview_workspace_file",
              path: file.path,
              max_bytes: 1_000_000,
            })
            if (requestId !== null) {
              this.#pendingFilePreview = { path: file.path, requestId }
            }
          },
        )
        break
      case "models":
        const models = this.#state.models.filter(
          (model) =>
            this.#modelProviderFilter === null ||
            (model.provider === undefined
              ? model.providers.includes(this.#modelProviderFilter)
              : model.provider === this.#modelProviderFilter),
        )
        const modelItems: PickerItem<ModelPickerChoice | null>[] =
          [
          ...(this.#modelProviderFilter === null
            ? this.#state.modelAliases.map((alias) => ({
                id: `alias:${alias.alias}`,
                label: `${alias.current ? "● " : ""}Alias · ${alias.alias}`,
                description: alias.candidates.join(" → "),
                value: { kind: "alias" as const, alias },
              }))
            : []),
          ...models.map((model) => ({
            id: model.id ?? model.alias,
            label: `${model.current === true ? "● " : ""}${model.displayName ?? model.alias}`,
            description: [
              model.provider ?? model.providers[0] ?? "unconfigured",
              model.available === false ? (model.status ?? "unavailable") : "available",
              model.toolCalling ? "tools" : "",
              model.vision ? "vision" : "",
              model.thinking ? "thinking" : "",
            ]
              .filter(Boolean)
              .join(" · "),
            value: { kind: "model" as const, model },
          })),
        ]
        const modelError = this.#projectionErrors.models
        if (modelError !== undefined) {
          modelItems.unshift({
            id: "models.error",
            label: "Couldn't load models",
            description: `${modelError} · select to retry`,
            value: null,
          })
        }
        if (modelItems.length === 0) {
          modelItems.push({
            id: "models.empty",
            label: "No configured model routes",
            description: "Configure a provider and model alias, then restart Rottweiler",
            value: null,
          })
        }
        this.#openPicker(
          this.#modelProviderFilter === null
            ? "Models"
            : `Models · ${this.#modelProviderFilter}`,
          modelItems,
          (item) => {
            const selection = item.value as ModelPickerChoice | null
            if (selection === null) {
              if (item.id === "models.error") {
                this.#requestModels()
                return
              }
              this.#projectClientError(
                "models_unavailable",
                "no configured model routes are available; configure a provider and model alias",
              )
              this.closePicker()
              return
            }
            if (selection.kind === "alias") {
              this.#command({
                type: "switch_model",
                model: selection.alias.alias,
                provider: null,
              })
              this.closePicker()
              return
            }
            const model = selection.model
            if (model.available === false) {
              this.#projectClientError(
                "model_unavailable",
                model.status ?? `${model.displayName ?? model.alias} is unavailable`,
                true,
              )
              return
            }
            this.#command({
              type: "switch_model",
              model: model.id ?? model.alias,
              provider: model.provider ?? this.#modelProviderFilter,
            })
            this.closePicker()
          },
        )
        break
      case "providers": {
        const providerChoices = this.#state.providers.length > 0
          ? this.#state.providers
          : [...new Set(this.#state.models.flatMap((model) => model.providers))].map((name) => ({
              name,
              authKind: "none" as const,
              nextAction: "select_models" as const,
              configured: true,
              authenticated: true,
              reachable: true,
              modelCount: this.#state.models.filter((model) => model.providers.includes(name)).length,
              status: null,
            }))
        const providerItems: PickerItem<RottweilerState["providers"][number] | null>[] =
          providerChoices
            .slice()
            .sort((left, right) => left.name.localeCompare(right.name))
            .map((provider) => ({
              id: provider.name,
              label: provider.name,
              description: [
                provider.authenticated ? "authenticated" : "not authenticated",
                provider.reachable ? "reachable" : "unreachable",
                `${provider.modelCount} model${provider.modelCount === 1 ? "" : "s"}`,
                provider.nextAction.replaceAll("_", " "),
                provider.status ?? "",
              ].filter(Boolean).join(" · "),
              value: provider,
            }))
        const providerError = this.#projectionErrors.models
        if (providerError !== undefined) {
          providerItems.unshift({
            id: "providers.error",
            label: "Couldn't load providers",
            description: `${providerError} · select to retry`,
            value: null,
          })
        }
        if (providerItems.length === 0) {
          providerItems.push({
            id: "providers.empty",
            label: "No configured provider routes",
            description: "Authenticate and configure a provider, then restart Rottweiler",
            value: null,
          })
        }
        this.#openPicker(
          "Providers",
          providerItems,
          (item) => {
            const provider = item.value as RottweilerState["providers"][number] | null
            if (provider === null) {
              if (item.id === "providers.error") {
                this.#requestModels()
                return
              }
              this.#projectClientError(
                "providers_unavailable",
                "no configured provider routes are available; authenticate and configure a provider",
              )
              this.closePicker()
              return
            }
            switch (provider.nextAction) {
              case "select_models":
                this.openModelPicker(provider.name)
                break
              case "authenticate":
                this.#command({ type: "begin_provider_auth", provider: provider.name })
                break
              case "api_key_cli":
                this.#projectClientError(
                  "provider_api_key_cli_required",
                  `API keys never enter the replayable UI protocol; run rw auth set-key ${provider.name}`,
                  true,
                )
                break
              case "configure":
                this.#command({ type: "configure_builtin_provider", provider: provider.name })
                break
              case "none":
                this.#projectClientError(
                  "provider_auth_unavailable",
                  provider.status ?? `${provider.name} has no safe authentication action`,
                  true,
                )
                break
            }
          },
        )
        break
      }
      case "providerAuth": {
        const pending = this.#state.providerAuth.pending
        if (pending === null) {
          this.openProviderPicker()
          break
        }
        const prompt = pending.challenge.kind === "oauth"
          ? `Open ${pending.challenge.authorization_url} · callback ${pending.challenge.redirect_uri}`
          : `Open ${pending.challenge.verification_uri} · enter code ${pending.challenge.user_code}`
        this.#openPicker(
          `Authenticate ${pending.provider}`,
          [
            {
              id: "provider-auth.waiting",
              label: "Waiting for authentication…",
              description: prompt,
              searchText: prompt,
              value: false,
            },
            {
              id: "provider-auth.cancel",
              label: "Cancel authentication",
              description: pending.warnings.join(" · "),
              value: true,
            },
          ],
          (item) => {
            if (item.value !== true) return
            this.#command({
              type: "cancel_provider_auth",
              provider: pending.provider,
              attemptId: pending.attemptId,
            })
          },
        )
        break
      }
      case "settings": {
        const items = this.#state.settings.flatMap((setting) =>
          setting.choices.map((value) => ({
            id: `${setting.key}:${value}`,
            label: `${setting.label} → ${value}`,
            description: `${value === setting.value ? "current · " : ""}${setting.provenance}${setting.appliesImmediately ? " · live" : " · next session"}`,
            value: { setting, value },
          })),
        )
        this.#openPicker("Settings", items, (item) => {
          const selection = item.value as {
            setting: RottweilerState["settings"][number]
            value: string
          }
          this.#command({
            type: "set_setting",
            key: selection.setting.key,
            value: selection.value,
          })
        })
        break
      }
      case "modes":
        this.#openPicker(
          "Modes",
          [
            {
              id: "execute",
              label: "Execute",
              description: "Use tools and make changes",
              value: "execute",
            },
            {
              id: "plan",
              label: "Plan",
              description: "Reason without mutations",
              value: "plan",
            },
            {
              id: "discuss",
              label: "Discuss",
              description: "Explore before acting",
              value: "discuss",
            },
          ],
          (item) => {
            this.#emit({
              type: "switch_mode",
              meta: this.#meta(),
              session_id: this.#sessionId,
              mode: item.value as ModeId,
            })
            this.closePicker()
          },
        )
        break
      case "sessions":
        const sessionError = this.#projectionErrors.sessions
        const sessionItems: PickerItem<RottweilerState["sessions"][number] | null>[] = [
          ...(sessionError === undefined
            ? []
            : [{
                id: "sessions.error",
                label: "Couldn't load sessions",
                description: `${sessionError} · select to retry`,
                value: null,
              }]),
          ...this.#state.sessions.map((session) => ({
            id: session.sessionId,
            label: session.workspaceName,
            description: `${session.model}${session.shellActive ? " · shell active" : ""}`,
            searchText: `${session.sessionId} ${session.workspaceName} ${session.model}`,
            value: session,
          })),
        ]
        this.#openPicker(
          this.#state.sessionSearch?.truncated === true
            ? "Sessions · results truncated"
            : "Sessions",
          sessionItems,
          (item) => {
            const session = item.value as RottweilerState["sessions"][number] | null
            if (session === null) {
              const query = this.picker.input.value.trim()
              this.#latestSessionsRequest =
                query.length === 0
                  ? this.#command({ type: "list_sessions" })
                  : this.#command({ type: "search_sessions", query, limit: 100 })
              return
            }
            void this.#options.onSessionSelect?.(session.sessionId)
            this.closePicker()
          },
        )
        break
      case null:
        break
    }
  }

  #openPicker<T>(
    title: string,
    items: readonly PickerItem<T>[],
    onSelect: (item: PickerItem<T>) => void,
  ): void {
    const select = (item: PickerItem<unknown>) => onSelect(item as PickerItem<T>)
    if (this.#pickerAnchored) {
      this.picker.refreshAnchored(
        title,
        items as readonly PickerItem<unknown>[],
        this.#pickerQuery,
        select,
      )
      this.#positionPicker(true)
      this.composer.focus()
    } else {
      this.picker.refresh(title, items as readonly PickerItem<unknown>[], select)
    }
    if (
      !this.#pickerAnchored &&
      this.#keybindings.preset === "vim" &&
      this.#vimFocus !== "picker"
    ) {
      this.#vimFocusBeforePicker = this.#vimFocus
      this.#vimFocus = "picker"
      this.#setInputMode("insert")
    }
  }


  #paletteActions(): readonly PaletteAction[] {
    const open = (action: () => void) => () => {
      this.closePicker()
      action()
    }
    const submit = (content: string) => () => {
      this.closePicker()
      if (
        this.#state.connection.phase === "connected" ||
        this.#state.connection.phase === "replaying"
      ) {
        void this.#sendMessage(content, [])
      } else {
        this.composer.value = content
        this.composer.focus()
      }
    }
    const prefill = (content: string) => () => {
      this.closePicker()
      this.composer.value = `${content} `
      this.composer.focus()
    }
    const actions: PaletteAction[] = [
      { id: "session.list", title: "Switch session", category: "Session", description: "Resume another durable session", run: open(() => this.openSessionPicker()) },
      { id: "model.list", title: "Switch model", category: "Agent", description: "Choose the active model alias", run: open(() => this.openModelPicker()) },
      { id: "provider.list", title: "Provider and model routes", category: "Agent", description: "Choose a configured provider route", run: open(() => this.openProviderPicker()) },
      { id: "settings.open", title: "Settings", category: "Settings", description: "Change safe persisted user settings", run: open(() => this.openSettingsPicker()) },
      { id: "mode.list", title: "Switch mode", category: "Agent", description: "Choose discuss, plan, or execute", run: open(() => this.openModePicker()) },
      { id: "review.open", title: "Review changes", category: "Session", description: "Open the cumulative session diff", run: open(() => this.openReview()) },
      { id: "permissions.manage", title: "Permission settings", category: "Settings", description: "Inspect approvals and session rules", run: prefill("/permissions") },
      { id: "permissions.list", title: "List permission rules", category: "Settings", description: "Show effective session permission rules", run: submit("/permissions list") },
      { id: "permissions.approvals", title: "List remembered approvals", category: "Settings", description: "Show remembered approval bindings", run: submit("/permissions approvals") },
      { id: "permissions.add", title: "Add permission rule", category: "Settings", description: "Add a session-scoped rule", run: prefill("/permissions add") },
      { id: "permissions.remove", title: "Remove permission rule", category: "Settings", description: "Remove a session-scoped rule", run: prefill("/permissions remove") },
      { id: "permissions.clear", title: "Clear session permissions", category: "Settings", description: "Clear this session's remembered rules", run: prefill("/permissions clear-session") },
      { id: "trust.manage", title: "Folder trust settings", category: "Settings", description: "Inspect, grant, or revoke workspace trust", run: prefill("/trust") },
      { id: "trust.status", title: "Show folder trust", category: "Settings", description: "Inspect the current workspace trust state", run: submit("/trust status") },
      { id: "trust.grant", title: "Trust this folder", category: "Settings", description: "Grant executable project configuration trust", run: prefill("/trust grant") },
      { id: "trust.revoke", title: "Revoke folder trust", category: "Settings", description: "Disable executable project configuration", run: prefill("/trust revoke") },
      { id: "context.manage", title: "Context manager", category: "Session", description: "Inspect, pin, or evict context", run: prefill("/context") },
      { id: "workspace.add", title: "Add workspace directory", category: "Workspace", description: "Append another live workspace root", run: prefill("/add-dir") },
      { id: "plan.show", title: "Show plan", category: "Session", description: "Display the pending or approved plan", run: submit("/plan") },
      { id: "cost.show", title: "Show usage and cost", category: "Session", description: "Display tokens, cost, and budget", run: submit("/cost") },
      { id: "compact.run", title: "Compact context", category: "Session", description: "Compact with optional instructions", run: prefill("/compact") },
      { id: "rewind.run", title: "Rewind to turn", category: "Session", description: "Restore a completed turn checkpoint", run: prefill("/rewind") },
      { id: "fork.run", title: "Fork session", category: "Session", description: "Fork at the latest completed turn", run: open(() => void this.#requestFork(null)) },
      { id: "interrupt.run", title: "Interrupt turn", category: "Session", description: "Stop the active turn", run: submit("/interrupt") },
      { id: "status.show", title: "Show agent status", category: "Agent", description: "Display running and queue state", run: submit("/status") },
      { id: "help.show", title: "Show command help", category: "System", description: "List every available slash command", run: submit("/help") },
    ]
    const mcpIndex = actions.findIndex((action) => action.id === "permissions.manage")
    if (this.#state.commands.some((command) => command.name === "mcp")) {
      actions.splice(
        mcpIndex,
        0,
        { id: "mcp.manage", title: "MCP connections", category: "Settings", description: "Inspect, enable, disable, or approve MCP servers", run: prefill("/mcp") },
        { id: "mcp.status", title: "Show MCP status", category: "Settings", description: "List every MCP connection and its state", run: submit("/mcp status") },
        { id: "mcp.enable", title: "Enable MCP server", category: "Settings", description: "Enable a configured MCP connection", run: prefill("/mcp enable") },
        { id: "mcp.disable", title: "Disable MCP server", category: "Settings", description: "Disable a configured MCP connection", run: prefill("/mcp disable") },
        { id: "mcp.approve", title: "Approve MCP server", category: "Settings", description: "Approve a displayed MCP fingerprint", run: prefill("/mcp approve") },
      )
    } else {
      actions.splice(mcpIndex, 0, {
        id: "mcp.configure",
        title: "Configure MCP connections",
        category: "Settings",
        description: "Add an MCP server configuration and restart Rottweiler",
        run: () => {
          this.closePicker()
          this.#projectClientError(
            "mcp_unconfigured",
            "no MCP servers are configured; add an MCP server configuration and restart Rottweiler",
          )
        },
      })
    }
    if (this.#state.commandsTruncated) {
      actions.push({
        id: "commands.truncated",
        title: "Command results truncated",
        category: "System",
        description: "The live extension catalog exceeded the safe display limit",
        run: () => {
          this.closePicker()
          this.#projectClientError(
            "command_catalog_truncated",
            "the live command catalog exceeded the safe display limit; narrow the configured extension set",
          )
        },
      })
    }
    const localNames = new Set(LOCAL_SLASH_COMMANDS.map((command) => command.name))
    for (const command of this.#state.commands) {
      if (localNames.has(command.name)) continue
      actions.push({
        id: `slash.${command.name}`,
        title: `Run /${command.name}`,
        category: commandSourceLabel(command.source),
        description: command.description,
        run: prefill(`/${command.name}`),
      })
    }
    return actions
  }

  #slashCommandChoices(): readonly CommandChoice[] {
    const choices = new Map(LOCAL_SLASH_COMMANDS.map((command) => [command.name, command]))
    for (const command of this.#state.commands) choices.set(command.name, command)
    return [...choices.values()]
  }

  #updateComposerAutocomplete(value: string): void {
    const slash = /^\/([^\s]*)$/.exec(value)
    if (slash !== null) {
      this.#pickerAnchored = true
      this.#pickerQuery = slash[1] ?? ""
      this.#positionPicker(true)
      this.#pickerKind = "commands"
      if (this.#state.commands.length === 0 && !this.#commandsRequested) {
        this.#requestCommands()
      }
      this.#refreshPicker()
      return
    }
    const mention = /(?:^|\s)@([^\s]*)$/.exec(value)
    if (mention === null && this.#pickerAnchored) this.closePicker()
  }

  #positionPicker(anchored: boolean): void {
    if (anchored) {
      const composerTop = Math.max(
        0,
        this.ctx.height - this.statusLine.height - this.composer.height,
      )
      this.picker.constrainAnchoredHeight(composerTop)
      this.picker.bottom = undefined
      this.picker.top = Math.max(0, composerTop - this.picker.height)
      this.picker.left = 0
      this.picker.width = "100%"
    } else {
      this.picker.bottom = undefined
      this.picker.top = 2
      this.picker.left = "15%"
      this.picker.width = "70%"
    }
  }

  #requestCommands(): void {
    this.#commandsRequested = true
    this.#clearProjectionError("commands")
    this.#latestCommandsRequest = this.#command({ type: "list_commands" })
  }

  #requestModels(refresh = false): void {
    this.#modelsRequested = true
    this.#clearProjectionError("models")
    this.#latestModelsRequest = this.#command({ type: "list_models", refresh })
  }

  #openChangedFileDiff(path: string): void {
    if (this.#state.replay.active || this.#state.shell.active) return
    this.#reviewOpen = true
    this.#pendingWorkspaceDiffPath = path
    this.reviewPanel.showWorkspaceDiffMessage(path, "Loading changed-file diff…")
    this.setState(this.#state)
    this.#latestWorkspaceDiffRequest = this.#command({
      type: "get_workspace_diff",
      path,
      max_bytes: 1_000_000,
    })
  }

  #scheduleSessionSearch(query: string): void {
    this.#clearSessionSearchTimer()
    this.#sessionSearchTimer = setTimeout(() => {
      this.#sessionSearchTimer = null
      if (this.#pickerKind === "sessions" && this.picker.input.value === query) {
        if (query.trim().length === 0) {
          this.#latestSessionsRequest = this.#command({ type: "list_sessions" })
        } else {
          this.#latestSessionsRequest = this.#command({ type: "search_sessions", query, limit: 100 })
        }
      }
    }, 80)
  }

  #clearSessionSearchTimer(): void {
    if (this.#sessionSearchTimer !== null) {
      clearTimeout(this.#sessionSearchTimer)
      this.#sessionSearchTimer = null
    }
  }

  async #sendMessage(content: string, attachments: readonly Attachment[]): Promise<boolean> {
    if (this.#state.replay.active) {
      return false
    }
    const sessionAction = attachments.length === 0 ? parseSessionAction(content) : null
    if (sessionAction?.type === "invalid") {
      this.#projectInvalidSlashCommand(sessionAction.message)
      return false
    }
    if (sessionAction?.type === "models") {
      this.#postSubmitPicker = "models"
      this.closePicker()
      return true
    }
    if (sessionAction?.type === "providers") {
      this.#postSubmitPicker = "providers"
      this.closePicker()
      return true
    }
    if (sessionAction?.type === "settings") {
      this.#postSubmitPicker = "settings"
      this.closePicker()
      return true
    }
    if (sessionAction?.type === "review") {
      if (this.#state.shell.active) {
        this.#projectClientError(
          "review_unavailable_during_shell",
          "exit the foreground shell before opening session review",
        )
        return false
      }
      this.reviewPanel.showSessionReview()
      this.#reviewOpen = true
      this.setState(this.#state)
      const meta = this.#meta()
      this.#latestReviewRequest = meta.request_id
      const outcome = await this.#emit({
        type: "get_session_review",
        meta,
        session_id: this.#sessionId,
      })
      if (outcome?.type !== "accepted") {
        this.#reviewOpen = false
        this.reviewPanel.closePresentation()
        this.setState(this.#state)
        this.#projectRejection(outcome)
      }
      return outcome?.type === "accepted"
    }
    if (sessionAction?.type === "fork") {
      return await this.#requestFork(sessionAction.atTurn)
    }
    if (content.startsWith("!")) {
      const command = content.slice(1).trim()
      if (command.length === 0 || attachments.length > 0) {
        return false
      }
      this.#suspendTerminal()
      this.#clearPendingShellTimer()
      this.#pendingShellTimer = setTimeout(() => {
        this.#pendingShellTimer = null
        if (!this.#state.shell.active) {
          this.#resumeTerminal()
        }
      }, 5_000)
      const outcome = await this.#emit({
        type: "user_shell_started",
        meta: this.#meta(),
        session_id: this.#sessionId,
        command,
      })
      if (outcome?.type !== "accepted") {
        this.#clearPendingShellTimer()
        if (!this.#state.shell.active) {
          this.#resumeTerminal()
        }
        this.#projectRejection(outcome)
        return false
      }
      return true
    }
    const outcome = await this.#emit({
      type: "send_message",
      meta: this.#meta(),
      session_id: this.#sessionId,
      content,
      attachments: [...attachments],
    })
    if (outcome?.type !== "accepted") {
      this.#projectRejection(outcome)
      return false
    }
    return true
  }

  #approve(tool: ToolProjection, decision: ApprovalDecision): void {
    void this.#submitApproval(tool, decision)
  }

  async #submitApproval(tool: ToolProjection, decision: ApprovalDecision): Promise<void> {
    try {
      const outcome = await this.#emit({
        type: "approve_tool",
        meta: this.#meta(),
        session_id: this.#sessionId,
        tool_call_id: tool.toolCallId,
        decision,
        binding: approvalBinding(tool.diff),
      })
      if (outcome?.type === "rejected") {
        this.#projectRejection(outcome)
      } else if (outcome === null) {
        this.#projectClientError(
          "tool_approval_unavailable",
          `the engine did not acknowledge the ${tool.name} approval decision`,
          true,
        )
      }
    } catch (error) {
      this.#projectClientError(
        "tool_approval_failed",
        `couldn't deliver the ${tool.name} approval decision: ${safeErrorMessage(error)}`,
        true,
      )
    }
  }

  #answer(question: QuestionProjection, values: readonly string[]): void {
    this.#emit({
      type: "answer_question",
      meta: this.#meta(),
      session_id: this.#sessionId,
      question_id: question.questionId,
      answers: [{ question_id: question.questionId, values: [...values] }],
    })
  }

  #reviewPlan(decision: PlanDecision): void {
    this.#emit({
      type: "approve_plan",
      meta: this.#meta(),
      session_id: this.#sessionId,
      decision,
      revisions: decision === "reject" ? "Revise the plan using the user's next message as feedback." : null,
    })
  }

  async #reviewFile(
    path: string,
    currentHash: string,
    decision: "accept" | "revert",
  ): Promise<void> {
    if (this.#state.shell.active) {
      this.#projectClientError(
        "review_unavailable_during_shell",
        "exit the foreground shell before deciding session review files",
      )
      return
    }
    if (this.#pendingReviewPaths.has(path)) return
    this.#pendingReviewPaths.add(path)
    this.reviewPanel.setDecisionPending(path, true)
    try {
      const outcome = await this.#emit({
        type: "review_file",
        meta: this.#meta(),
        session_id: this.#sessionId,
        path,
        decision,
        current_hash: currentHash,
      })
      if (outcome?.type === "rejected") {
        this.#projectRejection(outcome)
      } else if (outcome === null) {
        this.#projectClientError(
          "review_command_unavailable",
          "the review decision was not acknowledged by the engine",
          true,
        )
      }
    } catch {
      this.#projectClientError(
        "review_command_failed",
        "the review decision could not be delivered to the engine",
        true,
      )
    } finally {
      this.#pendingReviewPaths.delete(path)
      this.reviewPanel.setDecisionPending(path, false)
    }
  }

  async #requestFork(atTurn: string | null): Promise<boolean> {
    const meta = this.#meta()
    this.#pendingForkRequests.add(meta.request_id)
    const outcome = await this.#emit({
      type: "fork",
      meta,
      session_id: this.#sessionId,
      at_turn: atTurn,
    })
    if (outcome === null || outcome?.type === "rejected") {
      this.#pendingForkRequests.delete(meta.request_id)
    }
    if (outcome?.type === "rejected") this.#projectRejection(outcome)
    return outcome?.type === "accepted"
  }

  #closeReview(): void {
    if (!this.#reviewOpen) return
    this.#reviewOpen = false
    this.#pendingReviewSelection = null
    this.#pendingWorkspaceDiffPath = null
    this.reviewPanel.closePresentation()
    this.setState(this.#state)
    this.#focusForInputMode()
  }

  #command(
    command:
      | { readonly type: "search_workspace_files"; readonly query: string; readonly limit: number }
      | { readonly type: "preview_workspace_file"; readonly path: string; readonly max_bytes: number }
      | { readonly type: "switch_model"; readonly model: string; readonly provider: string | null }
      | { readonly type: "get_session_review" | "get_workspace_status" }
      | { readonly type: "get_workspace_diff"; readonly path: string; readonly max_bytes: number }
      | { readonly type: "search_sessions"; readonly query: string; readonly limit: number }
      | { readonly type: "list_models"; readonly refresh: boolean }
      | { readonly type: "list_settings" }
      | { readonly type: "set_setting"; readonly key: string; readonly value: string }
      | { readonly type: "begin_provider_auth"; readonly provider: string }
      | { readonly type: "configure_builtin_provider"; readonly provider: string }
      | { readonly type: "complete_provider_auth" | "cancel_provider_auth"; readonly provider: string; readonly attemptId: string }
      | { readonly type: "list_commands" | "list_sessions" },
  ): string | null {
    if (
      this.#state.replay.active &&
      command.type !== "list_sessions" &&
      command.type !== "search_sessions"
    ) {
      return null
    }
    const meta = this.#meta()
    if (command.type === "get_workspace_status") {
      this.#latestWorkspaceStatusRequest = meta.request_id
    } else if (command.type === "get_workspace_diff") {
      this.#latestWorkspaceDiffRequest = meta.request_id
    } else if (command.type === "get_session_review") {
      this.#latestReviewRequest = meta.request_id
    } else if (command.type === "switch_model") {
      if (this.#pendingModelSwitchRequests.size >= MAX_PENDING_MODEL_SWITCH_REQUESTS) {
        const oldest = this.#pendingModelSwitchRequests.values().next().value
        if (oldest !== undefined) this.#pendingModelSwitchRequests.delete(oldest)
      }
      if (this.#pendingModelSelections.size >= MAX_PENDING_MODEL_SWITCH_REQUESTS) {
        const oldest = this.#pendingModelSelections.keys().next().value
        if (oldest !== undefined) this.#pendingModelSelections.delete(oldest)
      }
      this.#pendingModelSwitchRequests.add(meta.request_id)
      const selection = command.model.includes("/") || command.provider === null
        ? command.model
        : `${command.provider}/${command.model}`
      this.#pendingModelSelections.set(meta.request_id, selection)
    } else if (command.type === "list_sessions" || command.type === "search_sessions") {
      this.#latestSessionsRequest = meta.request_id
    }
    let dispatched: ClientCommand
    switch (command.type) {
      case "list_models":
        dispatched = { ...command, meta, session_id: this.#sessionId }
        break
      case "list_sessions":
        dispatched = { type: command.type, meta }
        break
      case "list_commands":
      case "list_settings":
        dispatched = { type: command.type, meta, session_id: this.#sessionId }
        break
      case "set_setting":
        dispatched = { ...command, meta, session_id: this.#sessionId }
        break
      case "begin_provider_auth":
      case "configure_builtin_provider":
        dispatched = { ...command, meta, session_id: this.#sessionId }
        break
      case "complete_provider_auth":
      case "cancel_provider_auth":
        dispatched = {
          type: command.type,
          meta,
          session_id: this.#sessionId,
          provider: command.provider,
          attempt_id: command.attemptId,
        }
        break
      case "search_sessions":
        dispatched = { ...command, meta }
        break
      case "get_session_review":
      case "get_workspace_status":
        dispatched = { type: command.type, meta, session_id: this.#sessionId }
        break
      case "get_workspace_diff":
        dispatched = { ...command, meta, session_id: this.#sessionId }
        break
      case "search_workspace_files":
        dispatched = {
          ...command,
          meta,
          session_id: this.#sessionId,
        }
        break
      case "preview_workspace_file":
        dispatched = {
          ...command,
          meta,
          session_id: this.#sessionId,
        }
        break
      case "switch_model":
        dispatched = {
          ...command,
          meta,
          session_id: this.#sessionId,
        }
        break
    }
    void this.#emitProjectionCommand(command.type, dispatched, meta.request_id)
    return meta.request_id
  }

  async #emitProjectionCommand(
    type: ClientCommand["type"],
    command: ClientCommand,
    requestId: string,
  ): Promise<void> {
    try {
      const outcome = await this.#emit(command)
      if (outcome?.type === "rejected") {
        if (projectionKind(type) === null) {
          if (type === "switch_model") {
            this.#pendingModelSwitchRequests.delete(requestId)
            this.#pendingModelSelections.delete(requestId)
          }
          this.#projectRejection(outcome)
        } else {
          this.#recordProjectionFailure(type, requestId, outcome.error.message)
        }
      } else if (outcome === null) {
        const message = "the engine did not acknowledge the request"
        if (projectionKind(type) === null) {
          if (type === "switch_model") {
            this.#pendingModelSwitchRequests.delete(requestId)
            this.#pendingModelSelections.delete(requestId)
          }
          this.#projectClientError(`${type}_unavailable`, message, true)
        } else {
          this.#recordProjectionFailure(type, requestId, message)
        }
      }
    } catch (error) {
      const message = safeErrorMessage(error)
      if (projectionKind(type) === null) {
        if (type === "switch_model") {
          this.#pendingModelSwitchRequests.delete(requestId)
          this.#pendingModelSelections.delete(requestId)
        }
        this.#projectClientError(`${type}_failed`, message, true)
      } else {
        this.#recordProjectionFailure(type, requestId, message)
      }
    }
  }

  #recordProjectionFailure(type: ClientCommand["type"], requestId: string, message: string): void {
    const kind = projectionKind(type)
    if (kind === null || !this.#isCurrentProjectionRequest(kind, requestId)) return
    if (kind === "commands") {
      this.#commandsRequested = false
      this.#latestCommandsRequest = null
    } else if (kind === "models") {
      this.#modelsRequested = false
      this.#latestModelsRequest = null
    }
    this.#projectionErrors = { ...this.#projectionErrors, [kind]: message }
    this.#projectClientError(`${kind}_projection_failed`, `couldn't load ${kind}: ${message}`, true)
  }

  #isCurrentProjectionRequest(kind: ProjectionKind, requestId: string): boolean {
    if (kind === "commands") return this.#latestCommandsRequest === requestId
    if (kind === "models") return this.#latestModelsRequest === requestId
    if (kind === "sessions") return this.#latestSessionsRequest === requestId
    if (kind === "files") return this.#pendingWorkspaceSearchRequest === requestId
    return true
  }

  #clearProjectionError(kind: ProjectionKind): void {
    if (this.#projectionErrors[kind] === undefined) return
    const next = { ...this.#projectionErrors }
    delete next[kind]
    this.#projectionErrors = next
  }

  #meta() {
    return {
      protocol_version: PROTOCOL_VERSION,
      client_id: this.#options.clientId,
      request_id: this.#options.requestId(),
    }
  }

  async #emit(command: ClientCommand): Promise<void | CommandOutcome | null> {
    return this.#options.onCommand?.(command)
  }

  #suspendTerminal(): void {
    if (this.#terminalSuspended) {
      return
    }
    this.#options.terminalHandover?.suspend()
    this.#terminalSuspended = true
  }

  #resumeTerminal(): void {
    this.#clearPendingShellTimer()
    if (!this.#terminalSuspended) {
      return
    }
    this.#options.terminalHandover?.resume()
    this.#terminalSuspended = false
    this.composer.focus()
  }

  #clearPendingShellTimer(): void {
    if (this.#pendingShellTimer !== null) {
      clearTimeout(this.#pendingShellTimer)
      this.#pendingShellTimer = null
    }
  }

  #schedulePluginNotificationDismissal(
    notification: RottweilerState["pluginNotifications"][number] | undefined,
  ): void {
    this.#clearPluginNotificationTimer()
    if (notification === undefined) return
    this.#pluginNotificationTimer = setTimeout(() => {
      this.#pluginNotificationTimer = null
      if (this.#state.pluginNotifications.at(-1) !== notification) return
      this.setState({ ...this.#state, pluginNotifications: [] })
    }, 5_000)
  }

  #clearPluginNotificationTimer(): void {
    if (this.#pluginNotificationTimer !== null) {
      clearTimeout(this.#pluginNotificationTimer)
      this.#pluginNotificationTimer = null
    }
  }

  #projectRejection(outcome: void | CommandOutcome | null): void {
    if (outcome?.type !== "rejected") {
      return
    }
    this.setState({
      ...this.#state,
      errors: [...this.#state.errors.slice(-63), outcome.error],
    })
  }

  #projectInvalidSlashCommand(message: string): void {
    this.#projectClientError("invalid_command_arguments", message)
  }

  #projectClientError(code: string, message: string, retryable = false): void {
    this.setState({
      ...this.#state,
      errors: [
        ...this.#state.errors.slice(-63),
        {
          category: "protocol",
          code,
          message,
          retryable,
        },
      ],
    })
  }

  async #transitionToFork(childSessionId: string): Promise<void> {
    try {
      await this.#options.onSessionSelect?.(childSessionId)
    } catch {
      this.setState({
        ...this.#state,
        errors: [
          ...this.#state.errors.slice(-63),
          {
            category: "protocol",
            code: "fork_attach_failed",
            message: "the fork was created, but the TUI could not attach its child session",
            retryable: true,
          },
        ],
      })
    }
  }

  #notify(previous: RottweilerState, next: RottweilerState): void {
    if (this.#terminalFocused) {
      return
    }
    const finished = Object.values(next.turns).find(
      (turn) => turn.status !== "running" && previous.turns[turn.turnId]?.status === "running",
    )
    const approval = Object.values(next.tools).find(
      (tool) =>
        tool.status === "awaiting_approval" &&
        previous.tools[tool.toolCallId]?.status !== "awaiting_approval",
    )
    const question = Object.values(next.questions).find(
      (candidate) => !candidate.answered && previous.questions[candidate.questionId] === undefined,
    )
    const pluginNotification =
      next.pluginNotifications.at(-1) !== previous.pluginNotifications.at(-1)
        ? next.pluginNotifications.at(-1)
        : undefined
    if (pluginNotification !== undefined) {
      void this.#options.notifications.notify({
        kind: "plugin",
        title: pluginNotification.title,
        body: pluginNotification.message,
      })
    } else if (approval !== undefined) {
      void this.#options.notifications.notify({
        kind: "approval_needed",
        title: "Rottweiler needs approval",
        body: approval.name,
      })
    } else if (question !== undefined) {
      void this.#options.notifications.notify({
        kind: "question_asked",
        title: "Rottweiler has a question",
        body: question.questions[0]?.prompt ?? "Input required",
      })
    } else if (finished !== undefined) {
      void this.#options.notifications.notify({
        kind: "turn_finished",
        title: "Rottweiler finished",
        body: `Turn ${finished.turnId} · ${finished.status}`,
      })
    }
  }
}

type SessionAction =
  | { readonly type: "review" }
  | { readonly type: "fork"; readonly atTurn: string | null }
  | { readonly type: "models" }
  | { readonly type: "providers" }
  | { readonly type: "settings" }
  | { readonly type: "invalid"; readonly message: string }

function parseSessionAction(content: string): SessionAction | null {
  const tokens = content.trim().split(/\s+/)
  const command = tokens[0]
  if (command === "/review") {
    return tokens.length === 1
      ? { type: "review" }
      : { type: "invalid", message: "usage: /review" }
  }
  if (command === "/models") {
    return tokens.length === 1
      ? { type: "models" }
      : { type: "invalid", message: "usage: /models" }
  }
  if (command === "/providers") {
    return tokens.length === 1
      ? { type: "providers" }
      : { type: "invalid", message: "usage: /providers" }
  }
  if (command === "/settings") {
    return tokens.length === 1
      ? { type: "settings" }
      : { type: "invalid", message: "usage: /settings" }
  }
  if (command !== "/fork") return null
  if (tokens.length === 1) return { type: "fork", atTurn: null }
  if (tokens.length !== 2 || !isU64(tokens[1] ?? "")) {
    return { type: "invalid", message: "usage: /fork [turn] where turn is a decimal u64" }
  }
  return { type: "fork", atTurn: tokens[1] ?? null }
}

function isU64(value: string): boolean {
  return /^(0|[1-9][0-9]*)$/.test(value) && BigInt(value) <= 18_446_744_073_709_551_615n
}

function projectionKind(type: ClientCommand["type"]): ProjectionKind | null {
  switch (type) {
    case "list_commands":
      return "commands"
    case "list_models":
      return "models"
    case "list_sessions":
    case "search_sessions":
      return "sessions"
    case "search_workspace_files":
      return "files"
    default:
      return null
  }
}

function safeErrorMessage(error: unknown): string {
  return error instanceof Error && error.message.length > 0
    ? error.message
    : "the request could not be delivered to the engine"
}

function commandSourceLabel(source: CommandChoice["source"]): string {
  switch (source) {
    case "project": return "Project"
    case "user": return "User"
    case "plugin": return "Plugin"
    case "skill": return "Skills"
    case "workflow": return "Workflows"
    case "mcp": return "MCP"
    case "builtin":
    case undefined:
      return "Built-in"
  }
}

function approvalBinding(diff: unknown): ApprovalBinding | null {
  if (typeof diff !== "object" || diff === null) {
    return null
  }
  const value = diff as Record<string, unknown>
  if (
    typeof value.proposal_id !== "string" ||
    typeof value.arguments_hash !== "string" ||
    typeof value.base_hash !== "string" ||
    typeof value.diff_hash !== "string"
  ) {
    return null
  }
  return {
    proposal_id: value.proposal_id,
    arguments_hash: value.arguments_hash,
    base_hash: value.base_hash,
    diff_hash: value.diff_hash,
  }
}

/** Build the retained OpenTUI application tree. */
export function createRottweilerApp(
  renderer: RenderContext,
  options: RottweilerAppOptions = {},
): RottweilerApp {
  return new RottweilerApp(renderer, options)
}
