import { TodoController } from "./todo-controller"
import { notifyTransition } from "./app/notifications"
import { ClientRestoreController } from "./app/client-restore"
import { buildSurface } from "./app/surface"
import { SubmissionController } from "./app/submission"
import { PickerContentController, type PaletteAction } from "./app/picker-content"
import { InputUiController } from "./app/input"
import { ChildUiController } from "./app/children"
import { SessionUiController } from "./app/sessions"
import {
  BoxRenderable,
  CliRenderEvents,
  type RenderContext,
  type Selection,
  type ThemeMode,
  type TreeSitterClient,
} from "@opentui/core"
import { McpUiController } from "./app/mcp"
import type { RottweilerAppOptions } from "./app/options"
import { PermissionUiController } from "./app/permissions"
import { ProviderUiController } from "./app/provider"
import { SettingsUiController } from "./app/settings"
import { ThemeUiController } from "./app/themes"
import { DocumentController } from "./history/document"
import { HistoryPresentation } from "./history/presentation"
export type { RottweilerAppOptions,TerminalHandoverAdapter } from "./app/options"

import {
  ComposerRenderable,
  ContextPanelRenderable,
  FuzzyPickerRenderable,
  InteractionPanelRenderable,
  ListDetailRenderable,
  OutputViewerRenderable,
  ReviewPanelRenderable,
  StateBannerRenderable,
  StatusLineRenderable,
  SubagentTrayRenderable,
  ToolsWorkspaceRenderable,
  TranscriptRenderable,
} from "./components"
import { compileKeybindings } from "./keybindings"
import { type McpBrowserAction } from "./mcp-browser"
import { PickerController, type PickerCloseReason, type PickerKind } from "./picker-controller"
import { noExternalEditor, noExternalUrl, noImagePaste, noNotifications, noTextClipboard } from "./platform"
import { PresentationController, deferPresentationForEvent } from "./presentation"
import { ProjectionRequestBroker, type ProjectionKind } from "./projection-requests"
import { type CommandOutcome, type EngineEvent } from "./protocol"
import type { AppClientState } from "./recycle-state"
import {
  presentError,
  projectToolsWorkspace,
  sanitizeErrorFragment,
  type ToolsWorkspacePresentation,
} from "./render"
import { setWorkspaceRoots } from "./render/tool-presentation"
import { type SettingsBrowserAction } from "./settings-browser"
import {
  createInitialState,
  engineEvent,
  enterReplayMode,
  projectSessionTitleUpdate,
  reduceRottweilerState,
  type RottweilerState,
} from "./state"
import { childEngineEvent, childPassiveInteractionState } from "./subagent-state"
import { createSyntaxStyle, kennelTheme, systemThemeFor, themeByName, type RottweilerTheme } from "./theme"
import { durableSequenceId, isRecord } from "./transport"
import { stabilizeTreeSitterClient } from "./tree-sitter-client"
import { contextPanelHasContent } from "./ui-presentation"

export type { PresentationFrameScheduler } from "./presentation"

interface PendingPresentationEvent {
  readonly event: EngineEvent
  readonly eventRecord: Record<string, unknown>
  readonly commandRequestId: string | null
  readonly previous: RottweilerState
  readonly next: RottweilerState
}

export type { AppClientState } from "./recycle-state"

export type PrimaryView = "conversation" | "tools"

export class RottweilerApp extends BoxRenderable {
  transcript!: TranscriptRenderable
  toolsWorkspace!: ToolsWorkspaceRenderable
  contextPanel!: ContextPanelRenderable
  interactionPanel!: InteractionPanelRenderable
  outputViewer!: OutputViewerRenderable
  reviewPanel!: ReviewPanelRenderable
  picker!: FuzzyPickerRenderable<unknown>
  commandPalette!: ListDetailRenderable<PaletteAction>
  mcpBrowser!: ListDetailRenderable<McpBrowserAction>
  settingsBrowser!: ListDetailRenderable<SettingsBrowserAction>
  themeBrowser!: ListDetailRenderable<RottweilerTheme>
  composer!: ComposerRenderable
  statusLine!: StatusLineRenderable
  subagentTray!: SubagentTrayRenderable
  banner!: StateBannerRenderable
  main!: BoxRenderable

  readonly #pickerContent: PickerContentController
  readonly #submission: SubmissionController
  readonly #clientRestore: ClientRestoreController
  readonly #input: InputUiController
  readonly #children: ChildUiController
  readonly #sessions: SessionUiController
  readonly #themes: ThemeUiController
  readonly #settings: SettingsUiController
  readonly #permissions: PermissionUiController
  readonly #mcp: McpUiController
  readonly #providers: ProviderUiController
  readonly #document: DocumentController
  readonly #history: HistoryPresentation
  #state: RottweilerState
  #workspaceRoots: RottweilerState["workspaceRoots"] | undefined
  #options: Required<
    Pick<
      RottweilerAppOptions,
      | "sessionId"
      | "clientId"
      | "requestId"
      | "nowMs"
      | "notifications"
      | "editor"
      | "imagePaste"
      | "externalUrl"
      | "textClipboard"
    >
  > &
    RottweilerAppOptions
  #syntaxStyle!: ReturnType<typeof createSyntaxStyle>
  #theme: RottweilerTheme
  #treeSitterClient: TreeSitterClient | undefined
  #rethemeInProgress = false
  #composerSubmissionsInFlight = 0
  #deferredTheme: RottweilerTheme | null = null
  #sessionId: string
  #terminalFocused = true
  #systemThemeMode: ThemeMode | null
  #systemTheme: RottweilerTheme
  #todos: TodoController
  #projectionRequests: ProjectionRequestBroker
  #pickerController: PickerController
  #activeSubagentReadOnly = false
  #commandCatalogTruncationNotified = false
  #projectionErrors: Partial<Record<ProjectionKind, string>> = {}
  #outputViewerInvocationId: string | null = null
  #primaryView: PrimaryView = "conversation"
  #toolsElapsedTimer: ReturnType<typeof setInterval> | null = null
  #reviewOpen = false
  #pendingReviewSelection: string | null = null
  #pluginNotificationTimer: ReturnType<typeof setTimeout> | null = null
  #runtimeServicesTimer: ReturnType<typeof setTimeout> | null = null
  #clipboardNoticeTimer: ReturnType<typeof setTimeout> | null = null
  #destroyed = false
  #presentation: PresentationController<PendingPresentationEvent>
  #onTerminalFocus = () => {
    this.#terminalFocused = true
  }
  #onTerminalBlur = () => {
    this.#terminalFocused = false
    this.#input.clearInterruptEscape()
  }
  #onTerminalThemeMode = (mode: ThemeMode) => {
    this.#systemThemeMode = mode
    this.#systemTheme = systemThemeFor(mode)
    if (this.#theme.name !== "system") {
      const next = themeByName(this.#theme.name, mode)
      if (next !== undefined && next.mode !== this.#theme.mode) this.#createThemedSurface(next)
      return
    }
    // Palette refresh is owned by production startup. If the terminal changes
    // mode during a session, immediately switch to a safe matching fallback
    // rather than retaining an unreadable stale palette.
    const next = this.#systemTheme
    if (next.mode !== this.#theme.mode) this.#createThemedSurface(next)
  }
  #onSelection = (selection: Selection) => {
    const selectedText = selection.getSelectedText()
    if (selectedText.trim().length === 0) return
    void this.#options.textClipboard.writeText(selectedText).then(() => {
      if (this.#destroyed) return
      // Match OpenCode's copy-on-select contract: a completed drag copies once,
      // then releases the terminal selection so stale highlights cannot steal
      // later keyboard input. Re-evaluate the normal focus owner because the
      // selection may have crossed transcript, composer, or an interaction dock.
      // Clipboard writes are asynchronous. Only release the selection that
      // initiated this write; a newer drag must survive an older write
      // completing out of order.
      if (this.ctx.getSelection() === selection) {
        this.ctx.clearSelection()
        if (!this.#state.replay.active) this.#input.focusForInputMode()
      }
      this.#showClipboardNotice()
    }).catch(() => {
      if (this.#destroyed) return
      this.#projectClientError(
        "selection_copy_failed",
        "Couldn't copy the selected text to the clipboard.",
        true,
      )
    })
  }
  constructor(ctx: RenderContext, options: RottweilerAppOptions) {
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
      sessionId: options.replaySessionId ?? options.sessionId ?? "session-local",
      clientId: options.clientId ?? "tui-client",
      requestId: options.requestId ?? (() => crypto.randomUUID()),
      nowMs: options.nowMs ?? (() => Date.now()),
      notifications: options.notifications ?? noNotifications,
      editor: options.editor ?? noExternalEditor,
      imagePaste: options.imagePaste ?? noImagePaste,
      externalUrl: options.externalUrl ?? noExternalUrl,
      textClipboard: options.textClipboard ?? noTextClipboard,
    }
    this.#history = new HistoryPresentation(options.sessionReader, snapshot => {
      if (!this.#destroyed && this.transcript !== undefined) this.transcript.setHistory(snapshot)
    }, options.diagnostics)
    this.#document = new DocumentController(options.sessionReader, this.#history.controller.cache, snapshot => {
        if (!this.#destroyed && this.outputViewer !== undefined) {
          if (snapshot.open) this.outputViewer.showDocument(snapshot)
          else this.outputViewer.closePresentation()
        }
      })
    this.#projectionRequests = new ProjectionRequestBroker({
      clientId: () => this.#options.clientId,
      sessionId: () => this.#sessionId,
      requestId: () => this.#options.requestId(),
      replayActive: () => this.#state.replay.active,
      emit: (command) => this.#options.onCommand?.(command),
      onProjectionFailure: (kind, _type, _requestId, message) => {
        this.#recordProjectionFailure(kind, message)
      },
      onCommandFailure: (type, requestId, outcome, message, failure) => {
        if (outcome !== null) {
          this.#projectRejection(outcome)
          return
        }
        const code = `${type}_${failure === "unavailable" ? "unavailable" : "failed"}`
        this.#projectClientError(
          code,
          presentError({ category: "protocol", code, message, requestId }).text,
          true,
        )
      },
    })
    this.#todos = new TodoController({
      reader: options.sessionReader, state: () => this.#state.todos,
      update: (todos) => this.setState({ ...this.#state, todos }),
    })
    const inputApp = this
    this.#input = new InputUiController({
      get state() { return inputApp.#state }, get children() { return inputApp.#children },
      get sessions() { return inputApp.#sessions }, get document() { return inputApp.#document },
      get reviewOpen() { return inputApp.#reviewOpen }, get primaryView() { return inputApp.#primaryView },
      get pickerController() { return inputApp.#pickerController }, get requests() { return inputApp.#projectionRequests },
      get sessionId() { return inputApp.#sessionId }, get destroyed() { return inputApp.#destroyed },
      get theme() { return inputApp.#theme }, get platform() { return inputApp.#options.platform },
      get outputViewer() { return inputApp.outputViewer }, get reviewPanel() { return inputApp.reviewPanel },
      get interactionPanel() { return inputApp.interactionPanel }, get composer() { return inputApp.composer },
      get transcript() { return inputApp.transcript }, get toolsWorkspace() { return inputApp.toolsWorkspace },
      get statusLine() { return inputApp.statusLine }, get banner() { return inputApp.banner },
      get picker() { return inputApp.picker }, get mcpBrowser() { return inputApp.mcpBrowser },
      get settingsBrowser() { return inputApp.settingsBrowser }, get themeBrowser() { return inputApp.themeBrowser },
      get commandPalette() { return inputApp.commandPalette },
      discardPendingRestore: () => { this.#clientRestore.discard() },
      projectRejection: outcome => this.#projectRejection(outcome),
      projectError: (code, message, retryable) => this.#projectClientError(code, message, retryable),
      modelSupportsVision: state => this.#modelSupportsVision(state),
      closeOutputViewer: () => this.#closeOutputViewer(), closeReview: () => this.#closeReview(),
      closePicker: () => this.closePicker(), openSessionPicker: () => this.openSessionPicker(),
      openSubagentPicker: () => this.openSubagentPicker(), openReview: () => this.openReview(),
      openCommandPicker: () => this.openCommandPicker(), openModelPicker: () => this.openModelPicker(),
      openModePicker: () => this.openModePicker(),
    }, compileKeybindings(options.keybindings))
    this.#theme = theme
    this.#systemThemeMode = options.systemThemeMode ?? null
    this.#systemTheme = options.systemTheme ?? systemThemeFor(this.#systemThemeMode)
    this.#treeSitterClient = options.treeSitterClient === undefined
      ? undefined
      : stabilizeTreeSitterClient(options.treeSitterClient)
    this.#sessionId = this.#options.sessionId
    const initialState = options.initialState ?? createInitialState()
    this.#state =
      options.replaySessionId === undefined
        ? initialState
        : enterReplayMode(initialState, options.replaySessionId)
    if (this.#state.replay.active && this.#input.bindings.preset === "vim") {
      this.#input.restoreFocus("transcript")
    }
    if (options.initialEvent !== undefined) {
      this.#state = reduceRottweilerState(
        this.#state,
        engineEvent(options.initialEvent),
        this.#sessionId,
      )
    }
    this.#presentation = new PresentationController({
      scheduler: options.presentationFrame,
      diagnostics: options.diagnostics,
      destroyed: () => this.#destroyed,
      present: (pending, subagentDirty) => {
        const latest = pending.at(-1)
        if (latest !== undefined) this.#bindStateToComponents(latest.next)
        else if (subagentDirty) this.#bindStateToComponents(this.#state)
      },
      afterPresent: (item) => this.#afterPresentedEvent(item),
    })
    this.#pickerController = new PickerController({
      picker: () => this.picker,
      terminalHeight: () => this.height === 0 ? this.ctx.height : this.height,
      statusHeight: () => Math.max(1, this.statusLine.height || 1),
      composerDockHeight: () => this.composer.dockHeight,
      focusComposer: () => this.composer.focus(),
      renderPicker: () => this.#pickerContent.renderPicker(),
      withRefreshGuard: (kind, refresh) => {
        if (kind === "themes") this.#suppressThemePreview(refresh)
        else refresh()
      },
      onModalOpened: () => this.#modalOpened(),
      onClosed: (kind, reason) => this.#afterPickerClosed(kind, reason),
    })

    const app = this
    this.#children = new ChildUiController({
      sessionReader: options.sessionReader,
      get state() { return app.#state }, set state(value) { app.#state = value },
      get sessionId() { return app.#sessionId }, get composer() { return app.composer },
      get banner() { return app.banner }, get theme() { return app.#theme },
      get history() { return app.#history }, get diagnostics() { return app.#options.diagnostics },
      pickerController: this.#pickerController, requests: this.#projectionRequests,
      focus: () => this.#input.focusForInputMode(), refresh: () => this.setState(this.#state),
      presentEvent: event => this.#presentation.markDirty(deferPresentationForEvent(event)),
      closePicker: () => this.closePicker(), binding: action => this.#pickerContent.paletteBinding(action),
      projectError: (code, message, retryable) => this.#projectClientError(code, message, retryable),
      projectRejection: outcome => this.#projectRejection(outcome),
    })
    this.#sessions = new SessionUiController({
      sessionReader: options.sessionReader, historyCache: this.#history.controller.cache,
      drafts: this.#children.draftStore,
      get draftScope() { return app.#children.composerScope() },
      get state() { return app.#state }, get sessionId() { return app.#sessionId },
      get picker() { return app.picker }, get composer() { return app.composer },
      get banner() { return app.banner }, get theme() { return app.#theme },
      get projectionErrors() { return app.#projectionErrors }, get destroyed() { return app.#destroyed },
      get composerNotice() { return app.#submission.notice }, set composerNotice(value) { app.#submission.notice = value },
      pickerController: this.#pickerController, requests: this.#projectionRequests,
      refresh: () => this.setState(this.#state), closePicker: () => this.closePicker(),
      selectSession: id => this.#options.onSessionSelect?.(id),
      sendMessage: (content, attachments) => this.#submission.sendMessage(content, attachments),
      projectError: (code, message, retryable) => this.#projectClientError(code, message, retryable),
      projectRejection: outcome => this.#projectRejection(outcome),
    })
    this.#providers = new ProviderUiController({
      get state() { return app.#state },
      get activeSubagentId() { return app.#children.activeId },
      get draft() { return app.composer.value },
      get picker() { return app.picker },
      pickerController: this.#pickerController,
      requests: this.#projectionRequests,
      get projectionErrors() { return app.#projectionErrors },
      options: this.#options,
      closePicker: () => this.closePicker(),
      clearProjectionError: kind => this.#clearProjectionError(kind),
      projectError: (code, message, retryable) => this.#projectClientError(code, message, retryable),
    })
    this.#themes = new ThemeUiController({
      get theme() { return app.#theme }, get browser() { return app.themeBrowser },
      pickerController: this.#pickerController, requests: this.#projectionRequests,
      get sessionId() { return app.#sessionId }, get vim() { return app.#input.bindings.preset === "vim" },
      get terminalWidth() { return app.width || app.ctx.width }, get terminalHeight() { return app.height || app.ctx.height },
      get statusHeight() { return app.statusLine.height }, get composerDockHeight() { return app.composer.dockHeight },
      get deferred() { return app.#deferredTheme !== null }, get previewSuppressed() { return app.#rethemeInProgress },
      resolveTheme: theme => this.#resolvedTheme(theme), applyTheme: theme => this.#createThemedSurface(theme),
      withPreviewSuppressed: action => this.#suppressThemePreview(action),
      closePicker: () => this.closePicker(), modalOpened: () => this.#modalOpened(),
      projectRejection: outcome => this.#projectRejection(outcome),
      projectError: (code, message, retryable) => this.#projectClientError(code, message, retryable),
    })
    this.#settings = new SettingsUiController({
      get state() { return app.#state }, get picker() { return app.picker },
      get browser() { return app.settingsBrowser },
      pickerController: this.#pickerController, requests: this.#projectionRequests,
      get projectionErrors() { return app.#projectionErrors },
      get terminalWidth() { return app.width || app.ctx.width },
      get terminalHeight() { return app.height || app.ctx.height },
      get statusHeight() { return app.statusLine.height },
      get composerDockHeight() { return app.composer.dockHeight },
      get vim() { return app.#input.bindings.preset === "vim" },
      closePicker: () => this.closePicker(), openThemePicker: () => this.openThemePicker(),
      modalOpened: () => this.#modalOpened(),
    })
    this.#permissions = new PermissionUiController({
      get state() { return app.#state }, get picker() { return app.picker },
      pickerController: this.#pickerController, requests: this.#projectionRequests,
      get projectionErrors() { return app.#projectionErrors },
      closePicker: () => this.closePicker(), submitPaletteCommand: content => this.#pickerContent.submitPaletteCommand(content),
    })
    this.#mcp = new McpUiController({
      get state() { return app.#state },
      get picker() { return app.picker },
      get browser() { return app.mcpBrowser },
      pickerController: this.#pickerController,
      requests: this.#projectionRequests,
      get projectionErrors() { return app.#projectionErrors },
      get terminalWidth() { return app.width || app.ctx.width },
      get terminalHeight() { return app.height || app.ctx.height },
      get statusHeight() { return app.statusLine.height },
      get composerDockHeight() { return app.composer.dockHeight },
      get vim() { return app.#input.bindings.preset === "vim" },
      closePicker: () => this.closePicker(),
      modalOpened: () => this.#modalOpened(),
      projectError: (code, message, retryable) => this.#projectClientError(code, message, retryable),
    })
    this.#pickerContent = new PickerContentController({
      get terminalWidth() { return app.width || app.ctx.width }, get terminalHeight() { return app.height || app.ctx.height },
      ui: this, pickerController: this.#pickerController, input: this.#input,
      requests: this.#projectionRequests, children: this.#children, sessions: this.#sessions,
      providers: this.#providers, permissions: this.#permissions, settings: this.#settings,
      mcp: this.#mcp, themes: this.#themes,
      get projectionErrors() { return app.#projectionErrors }, get sessionId() { return app.#sessionId },
      onExit: () => this.#options.onExit?.(), modalOpened: () => this.#modalOpened(),
      clearProjectionError: kind => this.#clearProjectionError(kind),
      requestFork: turn => this.#submission.requestFork(turn), sendMessage: (content, attachments) => this.#submission.sendMessage(content, attachments),
    })
    this.#submission = new SubmissionController({
      ui: this, children: this.#children, sessions: this.#sessions, pickerContent: this.#pickerContent,
      requests: this.#projectionRequests, terminalHandover: this.#options.terminalHandover,
      get sessionId() { return app.#sessionId }, get destroyed() { return app.#destroyed },
      get reviewOpen() { return app.#reviewOpen }, set reviewOpen(value) { app.#reviewOpen = value },
      onExit: () => this.#options.onExit?.(), onComposerInput: value => this.#options.onComposerInput?.(value),
      projectError: (code, message, retryable) => this.#projectClientError(code, message, retryable),
      projectRejection: outcome => this.#projectRejection(outcome), invalidSlash: message => this.#projectInvalidSlashCommand(message),
    })
    this.#clientRestore = new ClientRestoreController({
      ui: this, history: this.#history.controller, pickerController: this.#pickerController, children: this.#children, sessions: this.#sessions,
      input: this.#input, providers: this.#providers, mcp: this.#mcp, themes: this.#themes,
      submission: this.#submission, pickerContent: this.#pickerContent,
      get submissionsInFlight() { return app.#composerSubmissionsInFlight }, get sessionId() { return app.#sessionId },
      get theme() { return app.#theme }, get reviewOpen() { return app.#reviewOpen },
      resolveTheme: theme => this.#resolvedTheme(theme), applyTheme: theme => this.#createThemedSurface(theme),
      setPrimaryView: view => this.#setPrimaryView(view), updateToolsWorkspace: (state, restore) => this.#updateToolsWorkspace(state, restore),
    })
    this.#createThemedSurface(theme)
    ctx.on(CliRenderEvents.FOCUS, this.#onTerminalFocus)
    ctx.on(CliRenderEvents.BLUR, this.#onTerminalBlur)
    ctx.on(CliRenderEvents.THEME_MODE, this.#onTerminalThemeMode)
    ctx.on(CliRenderEvents.SELECTION, this.#onSelection)
    ctx.keyInput.on("keypress", this.#input.onGlobalKey)
    this.setState(this.#state)
    if (this.#reviewOpen) {
      this.reviewPanel.files.focus()
    } else if (!this.#state.replay.active) {
      this.#input.focusForInputMode()
    }
  }

  #resumeDeferredTheme(): void {
    const deferred = this.#deferredTheme
    if (deferred === null || this.#composerSubmissionsInFlight > 0 || this.#children.draftStore.usage.pending > 0) return
    queueMicrotask(() => {
      if (!this.#destroyed && this.#composerSubmissionsInFlight === 0
        && this.#children.draftStore.usage.pending === 0 && this.#deferredTheme === deferred) {
        this.#createThemedSurface(deferred)
      }
    })
  }

  #createThemedSurface(theme: RottweilerTheme): void {
    const rebuilding = this.getChildrenCount() > 0
    if (rebuilding && (this.#composerSubmissionsInFlight > 0 || this.#children.draftStore.usage.pending > 0)) {
      this.#deferredTheme = theme
      return
    }
    this.#deferredTheme = null
    const composerState = rebuilding ? this.#clientRestore.captureComposerState() : null
    const transcriptClientState = rebuilding ? this.transcript.captureClientState() : null
    const toolsClientState = rebuilding ? this.toolsWorkspace.captureClientState() : null
    const toolsScrollTop = rebuilding ? this.toolsWorkspace.activityScroller.scrollTop : 0
    const scrollTop = rebuilding ? this.transcript.scroller.scrollTop : 0
    const pickerWasVisible = rebuilding && this.#input.pickerVisible()
    const pickerKind = this.#pickerController.kind
    const paletteWasVisible = pickerWasVisible && pickerKind === "palette"
    const mcpBrowserWasVisible = pickerWasVisible && pickerKind === "mcp"
    const settingsBrowserWasVisible = pickerWasVisible && pickerKind === "settings"
    const themeBrowserWasVisible = pickerWasVisible && pickerKind === "themes"
    const pickerQuery = rebuilding
      ? paletteWasVisible
        ? this.commandPalette.input.value
        : mcpBrowserWasVisible
          ? this.mcpBrowser.input.value
        : settingsBrowserWasVisible
          ? this.settingsBrowser.input.value
        : themeBrowserWasVisible
          ? this.themeBrowser.input.value
          : this.picker.input.value
      : ""
    const pickerSelection = rebuilding
      ? paletteWasVisible
        ? this.commandPalette.selectedId
        : mcpBrowserWasVisible
          ? this.mcpBrowser.selectedId
        : settingsBrowserWasVisible
          ? this.settingsBrowser.selectedId
        : themeBrowserWasVisible
          ? this.themeBrowser.selectedId
          : this.picker.select.getSelectedOption()?.value
      : undefined
    const pickerScrollOffset = rebuilding
      ? paletteWasVisible
        ? this.commandPalette.scrollOffset
        : mcpBrowserWasVisible
          ? this.mcpBrowser.scrollOffset
        : settingsBrowserWasVisible
          ? this.settingsBrowser.scrollOffset
        : themeBrowserWasVisible
          ? this.themeBrowser.scrollOffset
          : 0
      : 0
    if (rebuilding) {
      for (const child of this.getChildren()) {
        this.remove(child)
        child.destroyRecursively()
      }
      this.#syntaxStyle.destroy()
    }

    this.#rethemeInProgress = true
    this.#theme = theme
    this.backgroundColor = theme.background
    this.#syntaxStyle = createSyntaxStyle(theme)
    const app = this
    buildSurface({
      ui: this, context: this.ctx, options: this.#options, syntaxStyle: this.#syntaxStyle,
      retryTodos: () => this.#children.activeId === null ? this.#todos.retry() : this.#children.retryTodos(),
      treeSitterClient: this.#treeSitterClient, input: this.#input, children: this.#children,
      history: this.#history, document: this.#document, requests: this.#projectionRequests,
      submission: this.#submission, pickerController: this.#pickerController,
      sessions: this.#sessions, pickerContent: this.#pickerContent,
      get width() { return app.width || app.ctx.width }, get height() { return app.height || app.ctx.height },
      get outputViewerInvocationId() { return app.#outputViewerInvocationId },
      set outputViewerInvocationId(value) { app.#outputViewerInvocationId = value },
      openToolOutput: id => this.#openToolOutput(id), openChangedFileDiff: path => this.#openChangedFileDiff(path),
      closeReview: () => this.#closeReview(), resizeReviewPanel: (width, height) => this.#resizeReviewPanel(width, height),
      projectError: (code, message, retryable) => this.#projectClientError(code, message, retryable),
      onSubmit: async (content, submittedAttachments) => {
        this.#composerSubmissionsInFlight += 1
        return await this.#submission.sendMessage(content, submittedAttachments)
      },
      onInputSettled: () => this.#resumeDeferredTheme(),
      onSubmissionSettled: () => {
        this.#composerSubmissionsInFlight = Math.max(0, this.#composerSubmissionsInFlight - 1)
        this.#resumeDeferredTheme()
      },
    }, theme)
    this.setState(this.#state)
    if (this.#document.snapshot.open) this.outputViewer.showDocument(this.#document.snapshot)
    if (composerState !== null) this.#clientRestore.restoreComposerState(composerState)
    if (transcriptClientState !== null) this.transcript.restoreClientState(transcriptClientState)
    if (toolsClientState !== null) {
      this.#updateToolsWorkspace(this.#children.presentedState(), true)
      this.toolsWorkspace.restoreClientState(toolsClientState)
    }
    this.transcript.setScrollOffset(scrollTop)
    this.toolsWorkspace.activityScroller.scrollTo(toolsScrollTop)

    if (pickerWasVisible && pickerKind !== null) {
      this.#pickerController.query = pickerQuery
      this.#pickerController.refresh()
      if (pickerKind === "palette") {
        if (typeof pickerSelection === "string") this.commandPalette.selectById(pickerSelection)
        this.commandPalette.restoreViewport(pickerScrollOffset)
      } else if (pickerKind === "mcp") {
        if (typeof pickerSelection === "string") this.mcpBrowser.selectById(pickerSelection)
        this.mcpBrowser.restoreViewport(pickerScrollOffset)
      } else if (pickerKind === "settings") {
        if (typeof pickerSelection === "string") this.settingsBrowser.selectById(pickerSelection)
        this.settingsBrowser.restoreViewport(pickerScrollOffset)
      } else if (pickerKind === "themes") {
        if (typeof pickerSelection === "string") this.themeBrowser.selectById(pickerSelection)
        this.themeBrowser.restoreViewport(pickerScrollOffset)
      } else {
        const selectedIndex = this.picker.select.options.findIndex(
          (option) => option.value === pickerSelection,
        )
        if (selectedIndex >= 0) this.picker.select.setSelectedIndex(selectedIndex)
        this.picker.input.value = pickerQuery
      }
    }
    this.#rethemeInProgress = false
    if (this.mcpBrowser.visible) this.mcpBrowser.input.focus()
    else if (this.settingsBrowser.visible) this.settingsBrowser.input.focus()
    else if (this.themeBrowser.visible) this.themeBrowser.input.focus()
    else if (this.commandPalette.visible) this.commandPalette.input.focus()
    else if (this.picker.visible && !this.#pickerController.anchored) this.picker.input.focus()
  }

  get state(): RottweilerState {
    return this.#state
  }

  get primaryView(): PrimaryView {
    return this.#primaryView
  }

  get toolsElapsedTimerActive(): boolean {
    return this.#toolsElapsedTimer !== null
  }

  showToolsView(): void {
    this.#setPrimaryView("tools")
  }

  showConversationView(): void {
    this.#setPrimaryView("conversation")
  }

  get activeSubagentId(): string | null {
    return this.#children.activeId
  }

  /** Presentation state is exposed for focused UI tests; parent state remains `state`. */
  get visibleState(): RottweilerState {
    return this.#children.presentedState()
  }

  setSystemTheme(theme: RottweilerTheme): void {
    if (theme.name !== "system") return
    this.#systemTheme = theme
    this.#systemThemeMode = theme.mode
    if (this.#theme.name === "system" && theme.background !== this.#theme.background) {
      this.#createThemedSurface(theme)
    }
  }

  /** Update command routing only after the runtime owns the new driver lease. */
  setSessionId(sessionId: string): void {
    if (sessionId !== this.#sessionId) {
      this.closePicker("scope_change")
      this.#projectionRequests.clearForSessionChange()
      this.#todos.reset()
      this.#children.reset()
      this.#pickerContent.resetCommands()
      this.#commandCatalogTruncationNotified = false
      this.#providers.catalogSettled()
      this.#projectionErrors = {}
      this.#pendingReviewSelection = null
      this.#outputViewerInvocationId = null
      this.#reviewOpen = false
      this.#sessions.reset()
      this.#submission.reset()
      this.#providers.resetSession()
      this.outputViewer.closePresentation()
      this.reviewPanel.resetSession()
    }
    this.#sessionId = sessionId
    if (this.#state.replay.active && this.#state.replay.sessionId !== sessionId) {
      this.setState(enterReplayMode(createInitialState(), sessionId))
    }
  }

  resetConnectionProjections(): void {
    this.#projectionRequests.clearForReconnect()
    this.#todos.reset()
    this.#pickerContent.resetCommands()
    this.#providers.catalogSettled()
    this.#projectionErrors = {}
  }

  handleEvent(event: EngineEvent): void {
    if (this.#destroyed) return
    if (durableSequenceId(event) !== null && "meta" in event && isRecord(event.meta)
      && "session_id" in event.meta && typeof event.meta.session_id === "string") this.#history?.invalidate(event.meta.session_id)
    if (event.type === "session_history_ready" || event.type === "session_replay_completed") {
      this.#history?.invalidate(this.#sessionId)
    }
    const eventRecord = event as unknown as Record<string, unknown>
    const commandRequestId =
      isRecord(eventRecord.meta) && typeof eventRecord.meta.request_id === "string"
        ? eventRecord.meta.request_id
        : null
    if (event.type === "subagents_listed") {
      const listed = event as Extract<EngineEvent, { type: "subagents_listed" }>
      if (
        listed.session_id !== this.#sessionId ||
        !this.#projectionRequests.matches("subagents", commandRequestId)
      ) return
      this.#projectionRequests.clear("subagents")
      this.#children.acceptCatalog(listed.subagents)
      return
    }
    if (event.type === "subagent_progress") {
      const progress = event as Extract<EngineEvent, { type: "subagent_progress" }>
      if (progress.parent_session_id !== this.#sessionId) return
      const descriptor = this.#children.subagentDescriptor(progress.subagent_id)
      if (descriptor === undefined || descriptor.child_session_id !== progress.child_session_id) return
      const childEvent = childEngineEvent(progress.event, progress.child_session_id)
      if (childEvent !== null) {
        this.#history?.invalidate(progress.child_session_id)
        if (this.#children.activeId === progress.subagent_id) this.#children.applySubagentEvent(progress.subagent_id, childEvent)
      }
      const existing = this.#state.subagents[progress.subagent_id]
      if (existing === undefined || existing.childSessionId !== progress.child_session_id) return
    }
    if (event.type === "session_forked") {
      if (
        event.type !== "session_forked" ||
        event.parent_session_id !== this.#sessionId ||
        !this.#projectionRequests.acceptsFork(event.meta.request_id)
      ) return
      this.#projectionRequests.clearForks()
    }
    if (
      event.type === "sessions_search_ready" &&
      this.#pickerController.kind === "sessions" &&
      event.query !== this.picker.input.value
    ) {
      return
    }
    if (!this.#projectionRequests.acceptsEvent(event)) return
    const completedProjection = this.#projectionRequests.completeEvent(event)
    if (completedProjection === "commands") this.#pickerContent.resetCommands()
    if (completedProjection === "models") this.#providers.catalogSettled()
    if (completedProjection !== null) this.#clearProjectionError(completedProjection)
    const previous = this.#state
    const crossSessionTitle =
      event.type === "session_title_updated" &&
      isRecord(event.meta) &&
      event.meta.session_id !== this.#sessionId
    const reducedAt = this.#options.diagnostics?.start()
    let next =
      crossSessionTitle
        ? projectSessionTitleUpdate(
            previous,
            event as Extract<EngineEvent, { type: "session_title_updated" }>,
          )
        : reduceRottweilerState(previous, engineEvent(event), this.#sessionId)
    if (reducedAt !== undefined) this.#options.diagnostics?.finish("reducer", reducedAt)
    // Advance protocol state immediately so reconnect cursors and durable handoff
    // observe every accepted event even when its presentation waits for a frame.
    this.#state = next
    this.#presentation.enqueue(
      { event, eventRecord, commandRequestId, previous, next },
      deferPresentationForEvent(event),
    )
    this.#todos.event(event)
    if (event.type === "session_history_ready") this.#children.refreshTodos()
  }

  beginInitialReplayBatch(): void {
    // Historical events have already been reduced into durable state. Retain
    // only the newest projection while replaying so the queue cannot pin every
    // immutable intermediate transcript (or re-run historical UI effects).
    this.#presentation.suspend(true)
  }

  endInitialReplayBatch(): void {
    this.#presentation.resume()
    // Replay coalescing intentionally skips historical UI side effects, but
    // terminal ownership is current process state rather than historical
    // presentation. Reconcile it from the final durable projection so an
    // engine restart cannot leave OpenTUI reading while a foreground shell
    // still owns the supervisor's broker.
    if (!this.#state.replay.active && this.#state.shell.active) {
      this.#submission.clearPendingShellTimer()
      this.#submission.suspendTerminal()
    } else {
      this.#submission.resumeTerminal()
    }
  }

  #afterPresentedEvent(item: PendingPresentationEvent): void {
    const { event, eventRecord, commandRequestId, previous, next } = item
    if (
      event.type === "command_descriptors_listed" &&
      event.truncated &&
      !this.#commandCatalogTruncationNotified
    ) {
      this.#commandCatalogTruncationNotified = true
      this.#projectClientError(
        "command_catalog_truncated",
        "the live command catalog exceeded the safe display limit; narrow the configured extension set",
      )
    }
    this.#sessions.afterEvent(event, eventRecord, commandRequestId, next)
    const modelSwitchOutcome =
      event.type === "command_acknowledged" &&
      commandRequestId !== null &&
      this.#projectionRequests.consumeModelSwitch(commandRequestId)
        ? next.commandAcks[commandRequestId]?.outcome
        : null
    if (modelSwitchOutcome?.type === "rejected") {
      this.#projectRejection(modelSwitchOutcome)
    }
    this.#notify(previous, next)
    if (
      event.type === "question_asked" &&
      Array.isArray(eventRecord.questions) &&
      isRecord(eventRecord.questions[0]) &&
      eventRecord.questions[0].response_kind === "text"
    ) {
      this.composer.focus()
    }
    if (event.type === "session_forked") {
      void this.#transitionToFork(event.child.session_id)
    }
    if (next.pluginNotifications.at(-1) !== previous.pluginNotifications.at(-1)) {
      this.#schedulePluginNotificationDismissal(next.pluginNotifications.at(-1))
    }
    if (event.type === "user_shell_state_changed" && !next.replay.active) {
      if (event.active) {
        this.#submission.clearPendingShellTimer()
        this.#submission.suspendTerminal()
      } else {
        this.#submission.resumeTerminal()
      }
    }
    if (
      next.workspacePreview !== previous.workspacePreview &&
      next.workspacePreview !== null &&
      this.#projectionRequests.filePreview() !== null
    ) {
      const preview = next.workspacePreview
      const name = preview.path.split("/").filter(Boolean).at(-1) ?? "attachment"
      const attached = this.composer.addAttachment({
        name,
        source_path: preview.path,
        media_type: preview.mediaType,
        data: preview.data,
      })
      const pending = this.#projectionRequests.filePreview()!
      if (attached && pending.mention !== null) {
        const original = pending.draft.slice(pending.mention.start, pending.mention.end)
        const current = this.composer.value
        const draftOffset = current.indexOf(pending.draft)
        const exact = current === pending.draft
          ? pending.mention.start
          : draftOffset >= 0 && draftOffset === current.lastIndexOf(pending.draft)
            ? draftOffset + pending.mention.start
            : -1
        if (exact >= 0) this.composer.replaceRange(exact, exact + original.length, "")
      }
      this.#projectionRequests.setFilePreview(null)
      if (attached) this.closePicker()
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
      this.#projectionRequests.clear("workspace_diff")
      this.reviewPanel.showWorkspaceDiff(
        next.workspaceDiff.path,
        next.workspaceDiff.unifiedDiff,
        next.workspaceDiff.binary,
        next.workspaceDiff.truncated,
      )
    }
    if (event.type === "command_finished" && event.name === "add-dir" && !next.replay.active) {
      this.#pickerContent.requestCommands()
      this.#pickerContent.requestModes()
    }
    if (event.type === "subagent_spawned" || event.type === "subagent_finished") {
      this.#children.requestSubagents()
    }
    this.#providers.afterEvent(event, eventRecord, commandRequestId, next)

    if (
      event.type === "tool_call_finished" ||
      event.type === "turn_finished" ||
      event.type === "conversation_rewound" ||
      event.type === "session_review_updated" ||
      event.type === "command_finished" ||
      (event.type === "user_shell_state_changed" && !event.active)
    ) {
      this.#projectionRequests.command({ type: "get_workspace_status" })
    }
    if (
      event.type === "tool_call_started" ||
      event.type === "tool_call_finished"
    ) {
      this.#refreshRuntimeServicesWhileToolsRun()
    }
    if (
      event.type === "turn_finished" ||
      event.type === "conversation_rewound" ||
      event.type === "context_item_pinned" ||
      event.type === "context_item_evicted" ||
      event.type === "compaction_attempt_finished"
    ) {
      this.#projectionRequests.command({ type: "get_context" })
      this.#projectionRequests.command({ type: "get_cost" })
    }
  }

  setState(state: RottweilerState): void {
    if (this.#destroyed) return
    this.#presentation.flushBeforeStateChange()
    this.#bindStateToComponents(state)
  }

  recycleState(): AppClientState | null { return this.#clientRestore.recycleState() }
  restoreRecycleState(state: AppClientState): void { this.#clientRestore.restoreRecycleState(state) }
  applyPendingRecycleScroll(): void { this.#clientRestore.applyPendingRecycleScroll() }

  #modelSupportsVision(state: RottweilerState): boolean {
    const selected = state.models.find((model) => model.current && model.available !== false)
      ?? state.models.find(
        (model) => model.available !== false &&
          (model.id === state.model || model.aliases.includes(state.model ?? "")),
      )
    return selected?.vision === true
  }

  #bindStateToComponents(state: RottweilerState): void {
    const previousConnectionPhase = this.#state.connection.phase
    const previousFocusOwner = this.#input.visibleFocusOwner()
    if (state.workspaceRoots !== this.#workspaceRoots) {
      this.#workspaceRoots = state.workspaceRoots
      setWorkspaceRoots(state.workspaceRoots?.roots ?? [])
    }
    this.#state = state
    if (state.providerAuth.pending === null) {
      this.#providers.resetAuthentication()
    }
    const presented = this.#children.presentedState()
    this.composer.setImagePasteAvailable(this.#modelSupportsVision(presented))
    const viewingSubagent = this.#children.activeId !== null
    const childDescriptor = this.#children.activeId === null
      ? undefined
      : this.#children.subagentDescriptor(this.#children.activeId)
    this.transcript.update(
      presented,
      viewingSubagent ? childDescriptor?.agent || "Child agent" : "Rottweiler",
    )
    if (this.#history !== null) {
      this.#history.present(childDescriptor?.child_session_id ?? this.#children.historical?.sessionId ?? this.#sessionId)
      this.transcript.setHistory(this.#history.controller.snapshot)
    }
    this.#updateToolsWorkspace(presented)
    const viewedTool = this.#outputViewerInvocationId === null
      ? undefined
      : presented.tools[this.#outputViewerInvocationId]
    if (this.#outputViewerInvocationId !== null && viewedTool === undefined) {
      this.#outputViewerInvocationId = null
      this.outputViewer.closePresentation()
    } else if (viewedTool !== undefined) {
      if (this.outputViewer.invocationId === viewedTool.invocationId) {
        this.outputViewer.update(viewedTool)
      } else {
        this.outputViewer.open(viewedTool)
      }
    }
    this.subagentTray.update(state)
    this.contextPanel.update(presented)
    this.#applyPrimaryViewVisibility()
    this.subagentTray.setPresentationEnabled(!this.contextPanel.visible)
    this.interactionPanel.update(viewingSubagent ? childPassiveInteractionState(presented) : state)
    this.reviewPanel.update(state, !viewingSubagent && this.#reviewOpen)
    this.composer.setQueuedMessages(
      viewingSubagent || this.#primaryView === "tools" ? [] : state.queuedMessages,
    )
    const subagentReadOnly = this.#children.historical !== null || this.#children.isActiveSubagentRunning()
    const subagentBecameWritable = this.#activeSubagentReadOnly && !subagentReadOnly
    this.#activeSubagentReadOnly = subagentReadOnly
    const composerVisible =
      !state.replay.active &&
      !this.outputViewer.visible &&
      !subagentReadOnly &&
      (!this.interactionPanel.visible || this.interactionPanel.usesComposer)
    if (!composerVisible) this.composer.editor.blur()
    this.composer.visible = composerVisible
    this.interactionPanel.resizeForTerminal(
      this.height === 0 ? this.ctx.height : this.height,
      this.interactionPanel.usesComposer && composerVisible ? this.composer.dockHeight : 0,
    )
    const focusOwner = this.#input.visibleFocusOwner()
    if (
      (previousFocusOwner === "interaction" ||
        previousFocusOwner === "output" ||
        previousFocusOwner === "review") &&
      focusOwner !== "interaction" &&
      focusOwner !== "output" &&
      focusOwner !== "review"
    ) {
      this.#input.focusForInputMode()
    } else if (subagentBecameWritable) {
      this.#input.focusForInputMode()
    }
    this.statusLine.setBranch(viewingSubagent ? null : state.workspaceStatus?.branch ?? null)
    this.statusLine.setKeybindingMode(
      this.#input.mode === "standard" ? null : this.#input.mode,
      this.#input.mode === "standard" ? null : this.#input.statusFocusOwner(),
    )
    this.composer.setKeybindingMode(
      this.#input.mode === "standard" ? null : this.#input.mode,
    )
    this.statusLine.update(presented)
    this.banner.update(presented)
    if (this.#submission.notice !== null && this.composer.visible) {
      this.banner.visible = true
      this.banner.fg = this.#theme.textMuted
      this.banner.content = this.#submission.notice
    }
    if (viewingSubagent) this.#children.updateSubagentBanner(presented)
    if (!this.#input.isInterruptible()) this.#input.clearInterruptEscape(false)
    if (this.#input.escapeArmed) {
      this.banner.visible = true
      this.banner.fg = this.#theme.warning
      this.banner.content = this.#input.escapeChild === null
        ? "Press Esc again to stop the active response"
        : "Back in parent · press Esc again to stop the child agent"
    }
    if (this.#pickerController.kind !== null) {
      this.#pickerController.refresh()
    }
    if (state.connection.phase === "connected" && previousConnectionPhase !== "connected") {
      this.#history?.invalidate(childDescriptor?.child_session_id ?? this.#children.historical?.sessionId ?? this.#sessionId)
    }

  }

  openCommandPicker(): void { this.#pickerContent.openCommandPicker() }
  openKeyboardHelpPicker(): void { this.#pickerContent.openKeyboardHelpPicker() }
  openFilePicker(query = "", anchored = false): void { this.#pickerContent.openFilePicker(query, anchored) }
  openAttachmentPicker(): void { this.#pickerContent.openAttachmentPicker() }
  openWorkspaceRootsPicker(): void { this.#pickerContent.openWorkspaceRootsPicker() }
  openModePicker(): void { this.#pickerContent.openModePicker() }

  openModelPicker(provider: string | null = null): void { this.#providers.openModelPicker(provider) }

  openProviderPicker(onboarding = false): void { this.#providers.openProviderPicker(onboarding) }

  openProviderAuthPicker(): void { this.#providers.openProviderAuthPicker() }

  openProviderRecoveryPicker(provider: RottweilerState["providers"][number]): void { this.#providers.openProviderRecoveryPicker(provider) }

  openProviderApiKeyPrompt(provider: string): void { this.#providers.openProviderApiKeyPrompt(provider) }
  openSettingsPicker(): void { this.#settings.openSettingsPicker() }

  openPermissionPicker(): void { this.#permissions.openPermissionPicker() }

  openBudgetPicker(): void { this.#settings.openBudgetPicker() }

  openPermissionModePicker(): void { this.#permissions.openPermissionModePicker() }

  openTrustPicker(): void { this.#permissions.openTrustPicker() }

  openTimelinePicker(): void { this.#sessions.openTimelinePicker() }

  openQueuedMessagesPicker(): void { this.#sessions.openQueuedMessagesPicker() }

  openExportSessionPicker(): void { this.#sessions.openExportSessionPicker() }

  openMcpPicker(): void { this.#mcp.openMcpPicker() }
  openThemePicker(): void { this.#themes.openThemePicker() }

  #resizeReviewPanel(width: number, height: number): void {
    const primaryHeight = Math.max(
      1,
      height - this.statusLine.height - this.composer.dockHeight,
    )
    this.reviewPanel.resizeForTerminal(width, height, primaryHeight)
  }

  openSessionPicker(): void { this.#sessions.openSessionPicker() }

  openSubagentPicker(): void { this.#children.openSubagentPicker() }
  openSubagentActionPicker(subagentId = this.#children.activeId): void { this.#children.openSubagentActionPicker(subagentId) }

  #setPrimaryView(view: PrimaryView): void {
    if (this.#primaryView === view) {
      this.#applyPrimaryViewVisibility()
      this.#updateToolsWorkspace(this.#children.presentedState())
      return
    }
    this.#primaryView = view
    this.#applyPrimaryViewVisibility()
    this.#updateToolsWorkspace(this.#children.presentedState())
  }

  #applyPrimaryViewVisibility(): void {
    const toolsVisible = this.#primaryView === "tools"
    this.transcript.visible = !toolsVisible
    this.toolsWorkspace.visible = toolsVisible
    const width = this.width === 0 ? this.ctx.width : this.width
    const height = this.height === 0 ? this.ctx.height : this.height
    this.contextPanel.visible =
      !toolsVisible &&
      contextPanelHasContent(this.#children.presentedState()) &&
      width >= 100 &&
      height >= 12
    this.composer.setQueuedMessages(
      toolsVisible || this.#children.activeId !== null ? [] : this.#state.queuedMessages,
    )
    this.subagentTray.setPresentationEnabled(!this.contextPanel.visible)
  }

  #updateToolsWorkspace(state: RottweilerState, restoreHidden = false): void {
    if (this.#primaryView !== "tools" && !restoreHidden) {
      this.#clearToolsElapsedTimer()
      return
    }
    const model = projectToolsWorkspace(state, this.#options.nowMs())
    this.toolsWorkspace.update(model)
    this.#syncToolsElapsedTimer(model)
  }

  #syncToolsElapsedTimer(model: ToolsWorkspacePresentation): void {
    const hasKnownOpenTimer =
      (model.turn.kind === "running" && model.turn.elapsed.kind === "known") ||
      model.rows.some((row) =>
        row.kind === "tool" &&
        row.outcome.kind === "running" &&
        row.elapsed.kind === "known")
    const shouldRun =
      this.#primaryView === "tools" &&
      !model.replay &&
      hasKnownOpenTimer
    if (!shouldRun) {
      this.#clearToolsElapsedTimer()
      return
    }
    if (this.#toolsElapsedTimer !== null) return
    this.#toolsElapsedTimer = setInterval(() => {
      if (this.#destroyed || this.#primaryView !== "tools") {
        this.#clearToolsElapsedTimer()
        return
      }
      const presented = this.#children.presentedState()
      if (presented.replay.active) {
        this.#clearToolsElapsedTimer()
        return
      }
      const next = projectToolsWorkspace(presented, this.#options.nowMs())
      this.toolsWorkspace.update(next)
      if (
        next.turn.kind !== "running" &&
        !next.rows.some((row) =>
          row.kind === "tool" &&
          row.outcome.kind === "running" &&
          row.elapsed.kind === "known")
      ) {
        this.#clearToolsElapsedTimer()
      }
    }, 1_000)
  }

  #clearToolsElapsedTimer(): void {
    if (this.#toolsElapsedTimer === null) return
    clearInterval(this.#toolsElapsedTimer)
    this.#toolsElapsedTimer = null
  }

  #openToolOutput(invocationId: string): void {
    const tool = this.#children.presentedState().tools[invocationId]
    if (tool === undefined) return
    this.#document?.close()
    this.#outputViewerInvocationId = invocationId
    this.outputViewer.open(tool)
    this.setState(this.#state)
    this.outputViewer.focusPresentation()
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
    this.#resizeReviewPanel(
      this.width === 0 ? this.ctx.width : this.width,
      this.height === 0 ? this.ctx.height : this.height,
    )
    this.setState(this.#state)
    this.#projectionRequests.command({ type: "get_session_review" })
  }

  closePicker(reason: PickerCloseReason = "dismiss"): void {
    this.#clientRestore.discard()
    if (this.mcpBrowser.visible) this.mcpBrowser.close()
    if (this.settingsBrowser.visible) this.settingsBrowser.close()
    if (this.themeBrowser.visible) this.themeBrowser.close()
    if (this.commandPalette.visible) this.commandPalette.close()
    this.#pickerController.close(reason)
  }

  #suppressThemePreview(action: () => void): void {
    const previous = this.#rethemeInProgress
    this.#rethemeInProgress = true
    try { action() } finally { this.#rethemeInProgress = previous }
  }

  #modalOpened(): void { this.#input.modalOpened() }

  #afterPickerClosed(kind: PickerKind | null, reason: PickerCloseReason): void {
    const restoreSettingsBrowser = reason === "dismiss" && kind === "settingChoices"
    const restoreMcpBrowser = reason === "dismiss" && (kind === "mcpActions" || kind === "mcpInput" || kind === "mcpRemoveConfirm")
    this.#projectionRequests.clear("files")
    this.#projectionRequests.setFilePreview(null)
    this.#providers.pickerClosed()
    this.#mcp.pickerClosed()
    this.#settings.pickerClosed()
    this.#children.pickerClosed()
    this.#sessions.pickerClosed()
    if (restoreMcpBrowser) {
      this.#pickerController.kind = "mcp"
      this.mcpBrowser.visible = true
    } else if (restoreSettingsBrowser) {
      this.#pickerController.kind = "settings"
      this.settingsBrowser.visible = true
    }
    this.#input.modalClosed(restoreMcpBrowser || restoreSettingsBrowser)
    if (!this.#state.replay.active) this.#input.focusForInputMode()
    if (this.#input.bindings.preset === "vim") {
      this.statusLine.setKeybindingMode(
        this.#input.mode === "normal" ? "normal" : "insert",
        this.#input.statusFocusOwner(),
      )
      this.composer.setKeybindingMode(
        this.#input.mode === "normal" ? "normal" : "insert",
      )
      this.statusLine.update(this.#children.presentedState())
    }
  }

  protected override onResize(width: number, height: number): void {
    this.#applyPrimaryViewVisibility()
    this.composer.resizeForTerminal(height)
    this.interactionPanel.resizeForTerminal(
      height,
      this.interactionPanel.usesComposer && this.composer.visible ? this.composer.dockHeight : 0,
    )
    this.outputViewer.resizeForTerminal(height)
    this.#resizeReviewPanel(width, height)
    if (this.mcpBrowser.visible) this.#mcp.resize(width, height)
    else if (this.settingsBrowser.visible) this.#settings.resize(width, height)
    else if (this.themeBrowser.visible) this.#themes.resize(width, height)
    else if (this.commandPalette.visible) this.commandPalette.resizeForTerminal(width, height)
    else if (this.picker.visible) this.#pickerController.position(this.#pickerController.anchored)
  }

  override destroy(): void {
    if (this.#destroyed) return
    this.#destroyed = true
    this.#todos.dispose()
    this.#sessions.reset()
    this.#children.reset()
    this.#themes.dispose()
    this.#pickerController.dispose()
    this.#providers.dispose()
    this.#mcp.pickerClosed()
    this.#settings.pickerClosed()
    this.#document?.close()
    this.#history?.dispose()
    this.#presentation.destroy()
    this.#submission.reset()
    this.#clearPluginNotificationTimer()
    this.#clearRuntimeServicesTimer()
    this.#clearToolsElapsedTimer()
    this.#input.clearInterruptEscape(false)
    this.#clearClipboardNotice()
    this.ctx.off(CliRenderEvents.FOCUS, this.#onTerminalFocus)
    this.ctx.off(CliRenderEvents.BLUR, this.#onTerminalBlur)
    this.ctx.off(CliRenderEvents.THEME_MODE, this.#onTerminalThemeMode)
    this.ctx.off(CliRenderEvents.SELECTION, this.#onSelection)
    this.ctx.keyInput.off("keypress", this.#input.onGlobalKey)
    this.#syntaxStyle.destroy()
    super.destroy()
  }

  #showClipboardNotice(): void {
    this.#clearClipboardNotice()
    this.banner.visible = true
    this.banner.fg = this.#theme.success
    this.banner.content = "Copied to clipboard"
    this.#clipboardNoticeTimer = setTimeout(() => {
      this.#clipboardNoticeTimer = null
      if (!this.#destroyed) this.setState(this.#state)
    }, 1_500)
  }

  #clearClipboardNotice(): void {
    if (this.#clipboardNoticeTimer === null) return
    clearTimeout(this.#clipboardNoticeTimer)
    this.#clipboardNoticeTimer = null
  }

  #resolvedTheme(theme: RottweilerTheme): RottweilerTheme {
    if (theme.name === "system") return this.#systemTheme
    return themeByName(theme.name, this.#systemThemeMode ?? theme.mode) ?? theme
  }

  #openChangedFileDiff(path: string): void {
    if (this.#state.replay.active || this.#state.shell.active) return
    this.#reviewOpen = true
    this.#resizeReviewPanel(
      this.width === 0 ? this.ctx.width : this.width,
      this.height === 0 ? this.ctx.height : this.height,
    )
    this.reviewPanel.showWorkspaceDiffMessage(path, "Loading changed-file diff…")
    this.setState(this.#state)
    this.#projectionRequests.command({
      type: "get_workspace_diff",
      path,
      max_bytes: 1_000_000,
    })
  }

  #refreshRuntimeServicesWhileToolsRun(): void {
    this.#projectionRequests.command({ type: "list_runtime_services" })
    this.#clearRuntimeServicesTimer()
    if (!Object.values(this.#state.tools).some((tool) => tool.status === "running")) return
    this.#runtimeServicesTimer = setTimeout(() => {
      this.#runtimeServicesTimer = null
      if (this.#destroyed) return
      this.#refreshRuntimeServicesWhileToolsRun()
    }, 250)
  }

  #clearRuntimeServicesTimer(): void {
    if (this.#runtimeServicesTimer !== null) {
      clearTimeout(this.#runtimeServicesTimer)
      this.#runtimeServicesTimer = null
    }
  }

  #closeReview(): void {
    if (!this.#reviewOpen) return
    this.#reviewOpen = false
    this.#pendingReviewSelection = null
    this.#projectionRequests.clear("workspace_diff")
    this.reviewPanel.closePresentation()
    this.setState(this.#state)
    this.#input.focusForInputMode()
  }

  #closeOutputViewer(): void {
    if (!this.outputViewer.visible) return
    this.#document?.close()
    this.#outputViewerInvocationId = null
    this.outputViewer.closePresentation()
    this.setState(this.#state)
    this.#input.focusForInputMode()
  }

  #recordProjectionFailure(kind: ProjectionKind, message: string): void {
    if (kind === "commands") {
      this.#pickerContent.resetCommands()
    } else if (kind === "models") {
      this.#providers.catalogSettled()
    } else if (kind === "runtime_services" && this.#state.runtimeServices.length > 0) {
      this.setState({ ...this.#state, runtimeServices: [] })
    }
    const fragment = sanitizeErrorFragment(message)
    this.#projectionErrors = {
      ...this.#projectionErrors,
      [kind]: presentError({ message: fragment }).text,
    }
    const label = kind === "runtime_services" ? "active services" : kind
    this.#projectClientError(
      `${kind}_projection_failed`,
      `couldn't load ${label}: ${fragment}`,
      true,
    )
  }

  #clearProjectionError(kind: ProjectionKind): void {
    if (this.#projectionErrors[kind] === undefined) return
    const next = { ...this.#projectionErrors }
    delete next[kind]
    this.#projectionErrors = next
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
    if (!this.#terminalFocused) notifyTransition(this.#options.notifications, previous, next)
  }

}

/** Build the retained OpenTUI application tree. */
export function createRottweilerApp(
  renderer: RenderContext,
  options: RottweilerAppOptions,
): RottweilerApp {
  return new RottweilerApp(renderer, options)
}
