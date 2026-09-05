import { PickerContentController, type PaletteAction } from "./app/picker-content"
import { InputUiController } from "./app/input"
import { ChildUiController } from "./app/children"
import { SessionUiController } from "./app/sessions"
import {
BoxRenderable,
CliRenderEvents,
fg,
t,
type KeyEvent,
type RenderContext,
type Selection,
type ThemeMode,
type TreeSitterClient
} from "@opentui/core"
import { homedir } from "node:os"
import { McpUiController } from "./app/mcp"
import type { RottweilerAppOptions } from "./app/options"
import { PermissionUiController } from "./app/permissions"
import { ProviderUiController } from "./app/provider"
import { SettingsUiController } from "./app/settings"
import { ThemeUiController,themeBrowserDetail,themeBrowserRow } from "./app/themes"
import { DocumentController } from "./history/document"
import { HistoryPresentation } from "./history/presentation"
export type { RottweilerAppOptions,TerminalHandoverAdapter } from "./app/options"

import {
createCommandPaletteModel,
type CommandPaletteCatalog,
type CommandPaletteEntry,
} from "./command-palette"
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
formatSubagentElapsed,
type ListDetailPresentation,
type PickerItem
} from "./components"
import {
KEYBINDING_ACTION_LABELS,
compileKeybindings,
formatKeycap,
keyStrokeFromEvent,
legacyMacNavigationAction,
type CompiledKeybindings,
type InputMode,
type KeybindingAction,
type KeybindingContext,
type KeybindingPreset,
type VimFocus
} from "./keybindings"
import {
mcpBrowserRow,
type McpBrowserAction
} from "./mcp-browser"
import { PickerController,type PickerCloseReason,type PickerKind } from "./picker-controller"
import {
noExternalEditor,
noExternalUrl,
noImagePaste,
noNotifications,
noTextClipboard
} from "./platform"
import {
PresentationController
} from "./presentation"
import {
ProjectionRequestBroker,
type ProjectionKind,
} from "./projection-requests"
import {
type ApprovalBinding,
type ApprovalDecision,
type Attachment,
type CommandOutcome,
type EngineEvent,
type ModeId,
type PlanDecision
} from "./protocol"
import {
isRestorablePicker,
parseTuiRecycleState,
type AppClientState,
type ClientComposerState,
} from "./recycle-state"
import {
presentError,
projectToolsWorkspace,
sanitizeErrorFragment,
type ToolsWorkspacePresentation
} from "./render"
import { setWorkspaceRoots } from "./render/tool-presentation"
import {
commandSourceLabel,
isTuiHandledSlashCommand,
isU64,
mergeSlashCommandChoices,
parseSessionAction,
type CommandChoice,
} from "./session-commands"
import {
type SettingsBrowserAction
} from "./settings-browser"
import {
createInitialState,
engineEvent,
enterReplayMode,
projectSessionTitleUpdate,
reduceRottweilerState,
type QuestionProjection,
type RottweilerState,
type ToolProjection,
} from "./state"
import {
boundSubagentState,
childEngineEvent,
childPassiveInteractionState,
initialSubagentState,
mergeComposerDraft,
sanitizeSubagentDescriptor,
type ComposerDraft,
type SubagentDescriptor,
} from "./subagent-state"
import {
createSyntaxStyle,
kennelTheme,
systemThemeFor,
themeByName,
type RottweilerTheme
} from "./theme"
import {
durableSequenceId,
isRecord,
} from "./transport"
import { stabilizeTreeSitterClient } from "./tree-sitter-client"
import {
boundedUiText,
contextPanelHasContent,
modePickerPresentation,
nextModeId,
queuedMessageLabel,
timelineTurnLabel
} from "./ui-presentation"

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
  #projectionRequests: ProjectionRequestBroker
  #pickerController: PickerController
  #activeSubagentReadOnly = false
  #commandCatalogTruncationNotified = false
  #projectionErrors: Partial<Record<ProjectionKind, string>> = {}
  #outputViewerToolCallId: string | null = null
  #primaryView: PrimaryView = "conversation"
  #toolsElapsedTimer: ReturnType<typeof setInterval> | null = null
  #reviewOpen = false
  #pendingReviewSelection: string | null = null
  #postSubmitPicker: "models" | "providers" | "themes" | "settings" | "permissions" | "mcp" | "agents" | null = null
  #terminalSuspended = false
  #pendingShellTimer: ReturnType<typeof setTimeout> | null = null
  #pluginNotificationTimer: ReturnType<typeof setTimeout> | null = null
  #runtimeServicesTimer: ReturnType<typeof setTimeout> | null = null
  #clipboardNoticeTimer: ReturnType<typeof setTimeout> | null = null
  #pendingReviewPaths = new Set<string>()
  #composerNotice: string | null = null
  #lastComposerValue = ""
  #pendingClientState: AppClientState | null = null
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
      sessionId: options.sessionId ?? "session-local",
      clientId: options.clientId ?? "tui-client",
      requestId: options.requestId ?? (() => crypto.randomUUID()),
      nowMs: options.nowMs ?? (() => Date.now()),
      notifications: options.notifications ?? noNotifications,
      editor: options.editor ?? noExternalEditor,
      imagePaste: options.imagePaste ?? noImagePaste,
      externalUrl: options.externalUrl ?? noExternalUrl,
      textClipboard: options.textClipboard ?? noTextClipboard,
    }
    this.#history = new HistoryPresentation(options.historyReader, snapshot => {
      if (!this.#destroyed && this.transcript !== undefined) this.transcript.setHistory(snapshot)
    }, options.diagnostics)
    this.#document = new DocumentController(options.historyReader, this.#history.controller.cache, snapshot => {
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
      discardPendingRestore: () => { this.#pendingClientState = null },
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
      get state() { return app.#state }, get sessionId() { return app.#sessionId },
      get picker() { return app.picker }, get composer() { return app.composer },
      get banner() { return app.banner }, get theme() { return app.#theme },
      get projectionErrors() { return app.#projectionErrors }, get destroyed() { return app.#destroyed },
      get composerNotice() { return app.#composerNotice }, set composerNotice(value) { app.#composerNotice = value },
      pickerController: this.#pickerController, requests: this.#projectionRequests,
      refresh: () => this.setState(this.#state), closePicker: () => this.closePicker(),
      selectSession: id => this.#options.onSessionSelect?.(id),
      sendMessage: (content, attachments, preserve) => this.#sendMessage(content, attachments, preserve),
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
      requestFork: turn => this.#requestFork(turn), sendMessage: (content, attachments) => this.#sendMessage(content, attachments),
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

  #createThemedSurface(theme: RottweilerTheme): void {
    const rebuilding = this.getChildrenCount() > 0
    if (rebuilding && this.#composerSubmissionsInFlight > 0) {
      this.#deferredTheme = theme
      return
    }
    this.#deferredTheme = null
    const composerState = rebuilding ? this.#captureComposerState() : null
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
    this.banner = new StateBannerRenderable(this.ctx, theme)
    this.main = new BoxRenderable(this.ctx, {
      id: "main-content",
      width: "100%",
      flexGrow: 1,
      minHeight: 1,
      flexDirection: "row",
      backgroundColor: theme.background,
      gap: 0,
    })
    this.transcript = new TranscriptRenderable(this.ctx, theme, {
      diagnostics: this.#options.diagnostics,
      syntaxStyle: this.#syntaxStyle,
      ...(this.#treeSitterClient === undefined
        ? {}
        : { treeSitterClient: this.#treeSitterClient }),
      onInteraction: () => this.#input.restoreFocusAfterTranscriptInteraction(),
      onOpenSubagent: (subagentId) => {
        void this.#children.enterSubagent(subagentId)
      },
      onOpenChild: child => {
        this.#children.openHistorical({ sessionId: child.session_id, subagentId: child.subagent_id, task: child.task.text })
      },
      onOpenContent: source => {
        const view = this.#history?.controller.snapshot.page?.view
        if (view === undefined || this.#document === null) return
        this.#outputViewerToolCallId = null
        void this.#document.open(view, source)
        this.setState(this.#state)
        this.outputViewer.focusPresentation()
      },
      onHistoryAnchor: anchor => this.#history.controller.setAnchor(anchor),
      onHistorySeek: ordinal => { void this.#history?.controller.seek(ordinal) },
      onHistoryAround: item => { void this.#history?.controller.around(item) },
      onHistoryBoundary: boundary => { void this.#history?.controller.load({ type: boundary }) },
      onHistoryFollowing: following => this.#history?.controller.setFollowing(following),
      onOpenToolOutput: (toolCallId) => this.#openToolOutput(toolCallId),
    })
    this.toolsWorkspace = new ToolsWorkspaceRenderable(this.ctx, theme, {
      onOpenToolOutput: (toolCallId) => this.#openToolOutput(toolCallId),
    })
    this.toolsWorkspace.visible = this.#primaryView === "tools"
    this.contextPanel = new ContextPanelRenderable(this.ctx, theme, {
      onOpenDiff: (path) => this.#openChangedFileDiff(path),
      onOpenSubagent: (subagentId) => {
        void this.#children.enterSubagent(subagentId)
      },
    })
    this.main.add(this.transcript)
    this.main.add(this.toolsWorkspace)
    this.main.add(this.contextPanel)
    this.interactionPanel = new InteractionPanelRenderable(
      this.ctx,
      theme,
      this.#syntaxStyle,
      {
        onApproval: (tool, action) => {
          if (action === "allow_tool_session") {
            this.#projectionRequests.command({
              type: "add_session_permission_rule",
              pattern: `${tool.name}(*)`,
              action: "allow",
            })
            this.#approve(tool, "allow_once")
          } else if (action === "auto_safe_mode") {
            void this.#sendMessage("/permissions mode auto-safe", [])
            this.#approve(tool, "allow_once")
          } else {
            this.#approve(tool, action)
          }
        },
        onAnswer: (question, values) => this.#answer(question, values),
        onPlanReview: (decision) => this.#reviewPlan(decision),
      },
      this.#treeSitterClient,
    )
    this.reviewPanel = new ReviewPanelRenderable(
      this.ctx,
      theme,
      this.#syntaxStyle,
      {
        onDecision: (file, decision) =>
          void this.#reviewFile(file.path, file.currentHash, decision),
        onClose: () => this.#closeReview(),
      },
      this.#treeSitterClient,
    )
    this.outputViewer = new OutputViewerRenderable(this.ctx, theme)
    this.subagentTray = new SubagentTrayRenderable(
      this.ctx,
      theme,
      (subagentId) => void this.#children.enterSubagent(subagentId),
      () => {
        if (this.#children.activeId !== null) this.#children.updateSubagentBanner(this.#children.presentedState())
      },
    )
    const picker = new FuzzyPickerRenderable(this.ctx, theme, (query) => {
      if (this.picker !== picker) return
      if (this.#pickerController.kind === "sessions") this.#sessions.scheduleSessionSearch(query)
    })
    this.picker = picker
    this.picker.position = "absolute"
    this.picker.top = 2
    this.picker.left = "15%"
    this.picker.width = "70%"
    this.commandPalette = new ListDetailRenderable<PaletteAction>(this.ctx, theme)
    this.mcpBrowser = new ListDetailRenderable<McpBrowserAction>(this.ctx, theme, {
      surfaceLayout: "primary",
      splitListWidth: 72,
      splitMinWidth: 108,
      inputPlaceholder: "Filter MCP connections…",
      emptyCopy: "No MCP servers configured",
      surfaceBackground: theme.background,
      renderRow: (row, selected) => {
        const action = row.action
        const server = action.kind === "manage"
          ? this.#state.mcpServers.find((candidate) => candidate.name === action.server)
          : undefined
        return mcpBrowserRow(row, server, selected, theme)
      },
    })
    this.settingsBrowser = new ListDetailRenderable<SettingsBrowserAction>(this.ctx, theme, {
      surfaceLayout: "primary",
      splitListWidth: 29,
      splitMinWidth: 90,
      inputPlaceholder: "Filter settings…",
      emptyCopy: "No matching settings",
      surfaceBackground: theme.background,
    })
    this.themeBrowser = new ListDetailRenderable<RottweilerTheme>(this.ctx, theme, {
      surfaceLayout: "primary",
      splitListWidth: 33,
      splitMinWidth: 100,
      compactMinHeight: 8,
      inputPlaceholder: "Filter themes…",
      emptyCopy: "No matching themes",
      surfaceBackground: theme.background,
      renderRow: (row, selected, availableWidth) =>
        themeBrowserRow(row, selected, availableWidth, theme),
      renderDetail: (row) => themeBrowserDetail(row.action),
    })
    const pasteImageKeycap = this.#pickerContent.bindingHint("paste_image", ["global", this.#pickerContent.composerKeybindingContext()])
    const externalEditorKeycap = this.#pickerContent.bindingHint("open_external_editor", ["global", this.#pickerContent.composerKeybindingContext()])
    this.composer = new ComposerRenderable(this.ctx, theme, {
      editor: this.#options.editor,
      imagePaste: this.#options.imagePaste,
      ...(pasteImageKeycap === null ? {} : { pasteImageKeycap }),
      ...(externalEditorKeycap === null ? {} : { externalEditorKeycap }),
      onSubmit: async (content, submittedAttachments) => {
        this.#composerSubmissionsInFlight += 1
        return await this.#sendMessage(content, submittedAttachments)
      },
      submissionScope: () => this.#children.composerScope(),
      onDetachedSubmissionRejected: (scope, content, attachments) =>
        this.#children.restoreDetachedSubmission(scope, content, attachments),
      onFileMention: (mention) => this.openFilePicker(mention.query, true),
      onManageAttachments: () => this.openAttachmentPicker(),
      onAttachmentError: (message) =>
        this.#projectClientError("attachment_unavailable", message, true),
      onInput: (value) => this.#composerInputChanged(value),
      onSubmitted: () => this.#openPostSubmitPicker(),
      onSubmissionSettled: () => {
        this.#composerSubmissionsInFlight = Math.max(0, this.#composerSubmissionsInFlight - 1)
        const deferred = this.#deferredTheme
        if (deferred === null || this.#composerSubmissionsInFlight > 0) return
        queueMicrotask(() => {
          if (
            !this.#destroyed &&
            this.#composerSubmissionsInFlight === 0 &&
            this.#deferredTheme === deferred
          ) {
            this.#createThemedSurface(deferred)
          }
        })
      },
      onHeightChange: (height) => {
        this.interactionPanel.resizeForTerminal(
          this.height === 0 ? this.ctx.height : this.height,
          this.interactionPanel.usesComposer ? height : 0,
        )
        if (this.reviewPanel.visible && this.statusLine !== undefined) {
          this.#resizeReviewPanel(
            this.width === 0 ? this.ctx.width : this.width,
            this.height === 0 ? this.ctx.height : this.height,
          )
        }
      },
    })
    this.statusLine = new StatusLineRenderable(this.ctx, theme, {
      modelPickerKeycap: this.#pickerContent.bindingHint("open_model_picker", ["global"]),
    })
    this.add(this.banner)
    this.add(this.main)
    this.add(this.reviewPanel)
    this.add(this.outputViewer)
    this.add(this.interactionPanel)
    this.add(this.subagentTray)
    this.add(this.composer)
    this.add(this.statusLine)
    this.add(this.picker)
    this.add(this.commandPalette)
    this.add(this.mcpBrowser)
    this.add(this.settingsBrowser)
    this.add(this.themeBrowser)
    this.setState(this.#state)
    if (composerState !== null) this.#restoreComposerState(composerState)
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

  #openPostSubmitPicker(): void {
    const picker = this.#postSubmitPicker
    this.#postSubmitPicker = null
    if (picker === "models") this.openModelPicker()
    else if (picker === "providers") this.openProviderPicker()
    else if (picker === "themes") this.openThemePicker()
    else if (picker === "settings") this.openSettingsPicker()
    else if (picker === "permissions") this.openPermissionPicker()
    else if (picker === "mcp") this.openMcpPicker()
    else if (picker === "agents") this.openSubagentPicker()
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
      this.#children.reset()
      this.#pickerContent.resetCommands()
      this.#commandCatalogTruncationNotified = false
      this.#providers.catalogSettled()
      this.#projectionErrors = {}
      this.#pendingReviewSelection = null
      this.#outputViewerToolCallId = null
      this.#reviewOpen = false
      this.#sessions.reset()
      this.#composerNotice = null
      this.#providers.resetSession()
      this.outputViewer.closePresentation()
      this.reviewPanel.closePresentation()
    }
    this.#sessionId = sessionId
    if (this.#state.replay.active && this.#state.replay.sessionId !== sessionId) {
      this.setState(enterReplayMode(createInitialState(), sessionId))
    }
  }

  resetConnectionProjections(): void {
    this.#projectionRequests.clearForReconnect()
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
      this.#clearPendingShellTimer()
      this.#suspendTerminal()
    } else {
      this.#resumeTerminal()
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
        this.#clearPendingShellTimer()
        this.#suspendTerminal()
      } else {
        this.#resumeTerminal()
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

  #captureComposerState(): ClientComposerState {
    return {
      content: this.composer.value,
      attachments: [...this.composer.attachments],
      cursorOffset: this.composer.editor.cursorOffset,
      selection: this.composer.editor.getSelection(),
    }
  }

  #restoreComposerState(state: ClientComposerState): void {
    this.composer.restoreDraft(state.content, state.attachments)
    this.composer.editor.cursorOffset = state.cursorOffset
    if (state.selection !== null) this.composer.editor.setSelection(state.selection.start, state.selection.end)
  }

  #clientPickerSurface() {
    switch (this.#pickerController.kind) {
      case "palette": return this.commandPalette
      case "mcp": return this.mcpBrowser
      case "settings": return this.settingsBrowser
      case "themes": return this.themeBrowser
      default: return null
    }
  }

  /** Return no handoff while an interaction needs its current process or cannot fit the private cap. */
  recycleState(): AppClientState | null {
    const kind = this.#pickerController.kind
    if (this.#children.activeId !== null || this.#composerSubmissionsInFlight > 0
      || this.#terminalSuspended || this.#state.shell.active || this.#state.replay.active
      || this.#providers.hasPendingAction
      || this.#state.providerAuth.pending !== null || this.#mcp.hasDraft
      || this.#sessions.pending
      || this.#reviewOpen || this.outputViewer.visible || this.interactionPanel.visible
      || (kind !== null && !isRestorablePicker(kind))) return null
    const surface = this.#clientPickerSurface()
    const selected = surface?.selectedId ?? this.picker.select.getSelectedOption()?.value
    return parseTuiRecycleState({
      schemaVersion: 2,
      sessionId: this.#sessionId,
      composer: this.#captureComposerState(),
      subagentDrafts: [...this.#children.drafts],
      primaryView: this.#primaryView,
      scrollTop: Math.max(0, this.transcript.scroller.scrollTop),
      toolsScrollTop: Math.max(0, this.toolsWorkspace.activityScroller.scrollTop),
      transcript: this.transcript.captureClientState(),
      tools: this.toolsWorkspace.captureClientState(),
      inputMode: this.#input.mode,
      focus: this.#input.focus === "picker" ? this.#input.beforePicker : this.#input.focus,
      theme: this.#theme.name,
      picker: kind === null ? null : {
        kind,
        anchored: this.#pickerController.anchored,
        query: surface?.input.value ?? (this.#pickerController.anchored ? this.#pickerController.query : this.picker.input.value),
        selectedId: typeof selected === "string" ? selected : null,
        scrollOffset: surface?.scrollOffset ?? 0,
        modelProviderFilter: this.#providers.modelProviderFilter,
        onboarding: this.#providers.onboarding,
        themeBeforePreview: this.#themes.previewBase?.name ?? null,
      },
    })
  }

  /** Rebuild view bindings from client-owned data; projection responses remain engine-owned. */
  restoreRecycleState(state: AppClientState): void {
    if (state.sessionId !== this.#sessionId) return
    this.#providers.suppressOnboarding()
    const theme = this.#resolvedTheme(themeByName(state.theme) ?? kennelTheme)
    if (theme.name !== this.#theme.name) this.#createThemedSurface(theme)
    this.#restoreComposerState(state.composer)
    this.#children.restoreDrafts({ content: state.composer.content, attachments: state.composer.attachments }, state.subagentDrafts)
    this.#lastComposerValue = state.composer.content
    this.#input.restore(state.inputMode, state.focus)
    this.#setPrimaryView(state.primaryView)
    const picker = state.picker
    if (picker !== null) {
      switch (picker.kind) {
        case "palette": this.openCommandPicker(); break
        case "keyboardHelp": this.openKeyboardHelpPicker(); break
        case "commands": this.#pickerContent.requestCommands(); break
        case "attachments": this.openAttachmentPicker(); break
        case "mcp": this.openMcpPicker(); break
        case "modes": this.openModePicker(); break
        case "models": this.openModelPicker(picker.modelProviderFilter); break
        case "providers": this.openProviderPicker(picker.onboarding); break
        case "permissions": this.openPermissionPicker(); break
        case "permissionMode": this.openPermissionModePicker(); break
        case "trust": this.openTrustPicker(); break
        case "queuedMessages": this.openQueuedMessagesPicker(); break
        case "workspaceRoots": this.openWorkspaceRootsPicker(); break
        case "budgets": this.openBudgetPicker(); break
        case "sessions": this.openSessionPicker(); break
        case "settings": this.openSettingsPicker(); break
        case "agents": this.openSubagentPicker(); break
        case "timeline": this.openTimelinePicker(); break
        case "themes": this.openThemePicker(); break
      }
      this.#pickerController.begin(picker.kind, picker.anchored, picker.query)
      const surface = this.#clientPickerSurface()
      if (surface !== null) surface.input.value = picker.query
      else this.picker.input.value = picker.query
      this.#themes.restorePreviewBase(picker.themeBeforePreview === null
        ? null : this.#resolvedTheme(themeByName(picker.themeBeforePreview) ?? kennelTheme))
      this.#pickerController.refresh()
    }
    this.#pendingClientState = state
    this.setState(this.#state)
    this.#input.focusForInputMode()
  }

  /** Apply viewport/selection only after replay and OpenTUI layout have supplied their rows. */
  applyPendingRecycleScroll(): void {
    const state = this.#pendingClientState
    if (state === null) return
    const transcriptReady = state.scrollTop === 0 || this.transcript.mountedEntryCount > 0
    if (state.tools.expanded.length > 0 || state.tools.selectedId !== null || state.toolsScrollTop > 0) {
      this.#updateToolsWorkspace(this.#children.presentedState(), true)
    }
    const toolsReady = state.toolsScrollTop === 0 || this.toolsWorkspace.mountedRowCount > 0
    const transcriptBlocksReady = this.transcript.restoreClientState(state.transcript)
    const toolsBlocksReady = this.toolsWorkspace.restoreClientState(state.tools)
    if (transcriptReady) this.transcript.setScrollOffset(state.scrollTop)
    if (toolsReady) this.toolsWorkspace.activityScroller.scrollTo(state.toolsScrollTop)
    let pickerReady = true
    if (state.picker !== null && this.#pickerController.kind === state.picker.kind) {
      const surface = this.#clientPickerSurface()
      if (surface !== null) {
        if (state.picker.selectedId !== null) surface.selectById(state.picker.selectedId)
        pickerReady = state.picker.selectedId === null || surface.selectedId === state.picker.selectedId
        surface.restoreViewport(state.picker.scrollOffset)
      } else {
        const index = this.picker.select.options.findIndex((item) => item.value === state.picker?.selectedId)
        if (index >= 0) this.picker.select.setSelectedIndex(index)
        pickerReady = state.picker.selectedId === null || index >= 0
      }
    }
    if (transcriptReady && toolsReady && transcriptBlocksReady && toolsBlocksReady && pickerReady) this.#pendingClientState = null
  }

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
    const viewedTool = this.#outputViewerToolCallId === null
      ? undefined
      : presented.tools[this.#outputViewerToolCallId]
    if (this.#outputViewerToolCallId !== null && viewedTool === undefined) {
      this.#outputViewerToolCallId = null
      this.outputViewer.closePresentation()
    } else if (viewedTool !== undefined) {
      if (this.outputViewer.toolCallId === viewedTool.toolCallId) {
        this.outputViewer.update(viewedTool)
      } else {
        this.outputViewer.open(viewedTool)
      }
    }
    this.subagentTray.update(state)
    this.contextPanel.update(state)
    this.contextPanel.visible =
      this.#primaryView === "conversation" &&
      !viewingSubagent &&
      !state.replay.active &&
      contextPanelHasContent(state) &&
      (this.width === 0 ? this.ctx.width >= 100 : this.width >= 100)
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
    if (this.#composerNotice !== null && this.composer.visible) {
      this.banner.visible = true
      this.banner.fg = this.#theme.textMuted
      this.banner.content = this.#composerNotice
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
      this.#children.activeId === null &&
      !this.#state.replay.active &&
      contextPanelHasContent(this.#state) &&
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

  #openToolOutput(toolCallId: string): void {
    const tool = this.#children.presentedState().tools[toolCallId]
    if (tool === undefined) return
    this.#document?.close()
    this.#outputViewerToolCallId = toolCallId
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
    this.#pendingClientState = null
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
    this.#clearPendingShellTimer()
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

  #composerInputChanged(value: string): void {
    const changed = value !== this.#lastComposerValue
    this.#lastComposerValue = value
    if (!changed) {
      this.#pickerContent.updateComposerAutocomplete(value)
      return
    }
    this.#options.onComposerInput?.(value)
    this.transcript.clearBlockSelection()
    const hadPendingIntent = this.#sessions.clearRewind()
    const hadNotice = this.#composerNotice !== null
    this.#composerNotice = null
    if ((hadPendingIntent || hadNotice) && !this.#destroyed) this.setState(this.#state)
    this.#pickerContent.updateComposerAutocomplete(value)
  }

  #clearComposerNotice(): void {
    if (this.#composerNotice === null) return
    this.#composerNotice = null
    if (!this.#destroyed) this.setState(this.#state)
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

  async #sendMessage(
    content: string,
    attachments: readonly Attachment[],
    preserveRewindIntent = false,
  ): Promise<boolean> {
    if (!preserveRewindIntent) {
      this.#sessions.clearRewind()
      this.#clearComposerNotice()
    }
    if (this.#state.replay.active) {
      return false
    }
    if (content.startsWith("!")) {
      const originatingSubagentId = this.#children.activeId
      const accepted = await this.#startForegroundShell(content, attachments)
      if (accepted && originatingSubagentId !== null && this.#children.activeId === originatingSubagentId) {
        this.#children.leaveSubagent()
      }
      return accepted
    }
    if (this.#children.activeId !== null) {
      const action = attachments.length === 0 ? parseSessionAction(content) : null
      if (action?.type === "exit") {
        this.#options.onExit?.()
        return true
      }
      if (action?.type === "agents") {
        this.#postSubmitPicker = "agents"
        this.closePicker()
        return true
      }
      if (attachments.length > 0) {
        this.#projectClientError(
          "subagent_attachments_unsupported",
          "Child follow-ups are text-only; remove attachments or return to the parent session.",
        )
        return false
      }
      const subagentId = this.#children.activeId
      if (this.#children.subagentDescriptor(subagentId)?.activity === "running") {
        this.#projectClientError(
          "subagent_still_running",
          "This child is still working. Inspect its progress or interrupt it before sending a follow-up.",
        )
        return false
      }
      let outcome: void | CommandOutcome | null
      try {
        outcome = await this.#projectionRequests.emit({
          type: "continue_subagent",
          meta: this.#projectionRequests.meta(),
          session_id: this.#sessionId,
          subagent_id: subagentId,
          content,
        })
      } catch (error) {
        this.#projectClientError(
          "subagent_continue_failed",
          presentError({
            category: "protocol",
            code: "subagent_continue_failed",
            message: safeErrorMessage(error),
          }).text,
          true,
        )
        return false
      }
      if (outcome?.type !== "accepted") {
        if (outcome?.type === "rejected") this.#projectRejection(outcome)
        else {
          const presentation = presentError({
            category: "protocol",
            code: "subagent_continue_unavailable",
            message: "Couldn't continue the child because the engine connection is unavailable.",
          })
          this.#projectClientError("subagent_continue_unavailable", presentation.text, true)
        }
        return false
      }
      this.#children.responseStarted(subagentId)
      this.setState(this.#state)
      return true
    }
    const textQuestion = Object.values(this.#state.questions).find(
      (question) => !question.answered && question.questions[0]?.response_kind === "text",
    )
    if (textQuestion !== undefined) {
      if (attachments.length > 0) {
        this.#projectClientError(
          "question_attachments_unsupported",
          "Answer this question with text only; attachments stay in your draft.",
        )
        return false
      }
      const outcome = await this.#projectionRequests.emit({
        type: "answer_question",
        meta: this.#projectionRequests.meta(),
        session_id: this.#sessionId,
        question_id: textQuestion.questionId,
        answers: [{ question_id: textQuestion.questionId, values: [content] }],
      })
      if (outcome?.type !== "accepted") {
        this.#projectRejection(outcome)
        return false
      }
      return true
    }
    const sessionAction = attachments.length === 0 ? parseSessionAction(content) : null
    if (sessionAction?.type === "invalid") {
      this.#projectInvalidSlashCommand(sessionAction.message)
      return false
    }
    if (sessionAction?.type === "exit") {
      this.closePicker()
      this.#options.onExit?.()
      return true
    }
    if (sessionAction?.type === "new") {
      this.closePicker()
      void this.#sessions.createSession()
      return true
    }
    if (sessionAction?.type === "rewindTimeline") {
      this.closePicker()
      this.openTimelinePicker()
      return true
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
    if (sessionAction?.type === "agents") {
      this.#postSubmitPicker = "agents"
      this.closePicker()
      return true
    }
    if (sessionAction?.type === "theme") {
      this.#postSubmitPicker = "themes"
      this.closePicker()
      return true
    }
    if (sessionAction?.type === "settings") {
      this.#postSubmitPicker = "settings"
      this.closePicker()
      return true
    }
    if (sessionAction?.type === "permissions") {
      this.#postSubmitPicker = "permissions"
      this.closePicker()
      return true
    }
    if (sessionAction?.type === "mcp") {
      this.#postSubmitPicker = "mcp"
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
      const meta = this.#projectionRequests.issue("review")
      const outcome = await this.#projectionRequests.emit({
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
    const meta = this.#projectionRequests.meta()
    if (preserveRewindIntent) this.#sessions.bindRewindRequest(meta.request_id)
    const outcome = await this.#projectionRequests.emit({
      type: "send_message",
      meta,
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

  async #startForegroundShell(
    content: string,
    attachments: readonly Attachment[],
  ): Promise<boolean> {
    const command = content.slice(1).trim()
    if (command.length === 0 || attachments.length > 0) return false
    this.#suspendTerminal()
    this.#clearPendingShellTimer()
    this.#pendingShellTimer = setTimeout(() => {
      this.#pendingShellTimer = null
      if (!this.#state.shell.active) this.#resumeTerminal()
    }, 5_000)
    const outcome = await this.#projectionRequests.emit({
      type: "user_shell_started",
      meta: this.#projectionRequests.meta(),
      session_id: this.#sessionId,
      command,
    })
    if (outcome?.type !== "accepted") {
      this.#clearPendingShellTimer()
      if (!this.#state.shell.active) this.#resumeTerminal()
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
      const outcome = await this.#projectionRequests.emit({
        type: "approve_tool",
        meta: this.#projectionRequests.meta(),
        session_id: this.#sessionId,
        tool_call_id: tool.toolCallId,
        invocation_id: tool.invocationId,
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
        presentError({
          category: "protocol",
          code: "tool_approval_failed",
          message: safeErrorMessage(error),
        }).text,
        true,
      )
    }
  }

  #answer(question: QuestionProjection, values: readonly string[]): void {
    this.#projectionRequests.emit({
      type: "answer_question",
      meta: this.#projectionRequests.meta(),
      session_id: this.#sessionId,
      question_id: question.questionId,
      answers: [{ question_id: question.questionId, values: [...values] }],
    })
  }

  #reviewPlan(decision: PlanDecision): void {
    this.#projectionRequests.emit({
      type: "approve_plan",
      meta: this.#projectionRequests.meta(),
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
      const outcome = await this.#projectionRequests.emit({
        type: "review_file",
        meta: this.#projectionRequests.meta(),
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
    const meta = this.#projectionRequests.meta()
    this.#projectionRequests.trackFork(meta.request_id)
    const outcome = await this.#projectionRequests.emit({
      type: "fork",
      meta,
      session_id: this.#sessionId,
      at_turn: atTurn,
      operation_id: crypto.randomUUID(),
    })
    if (outcome === null || outcome?.type === "rejected") {
      this.#projectionRequests.discardFork(meta.request_id)
    }
    if (outcome?.type === "rejected") this.#projectRejection(outcome)
    return outcome?.type === "accepted"
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
    this.#outputViewerToolCallId = null
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

function safeErrorMessage(error: unknown): string {
  return error instanceof Error && error.message.length > 0
    ? error.message
    : "the request could not be delivered to the engine"
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

const IMMEDIATE_PRESENTATION_EVENTS = new Set<EngineEvent["type"]>([
  "command_acknowledged",
  "context_snapshot_ready",
  "cost_snapshot_ready",
  "session_review_ready",
  "session_review_updated",
  "prompt_dump_ready",
  "session_replay_completed",
  "session_history_ready",
  "session_forked",
  "session_exported",
  "sessions_listed",
  "subagents_listed",
  "command_descriptors_listed",
  "models_listed",
  "modes_listed",
  "settings_listed",
  "permissions_listed",
  "mcp_servers_listed",
  "runtime_services_listed",
  "workspace_files_found",
  "workspace_roots_changed",
  "workspace_status_ready",
  "sessions_search_ready",
  "workspace_file_preview_ready",
  "workspace_diff_ready",
  "host_shutdown",
  "ui_notification",
  "conversation_rewound",
  "conversation_turn_committed",
  "tool_approval_needed",
  "question_asked",
  "question_answered",
  "tool_call_started",
  "tool_call_finished",
  "tool_diff_ready",
  "tool_output_pruned",
  "turn_started",
  "turn_finished",
  "user_message_accepted",
  "message_queued",
  "queued_message_removed",
  "queued_messages_cleared",
  "user_shell_state_changed",
  "command_finished",
  "mode_changed",
  "model_changed",
  "model_context_cleared",
  "driver_changed",
  "permission_mode_changed",
  "budget_status_changed",
  "context_item_pinned",
  "context_item_evicted",
  "compaction_started",
  "compaction_finished",
  "compaction_failed",
  "compaction_attempt_started",
  "compaction_attempt_finished",
  "plan_submitted",
  "plan_reviewed",
  "subagent_spawned",
  "subagent_finished",
  "provider_configured",
  "provider_activation_finished",
  "provider_auth_started",
  "provider_auth_finished",
  "mcp_server_approval_reviewed",
  "plugin_message_injected",
  "plugin_status_changed",
  "session_created",
  "session_title_updated",
  "guard_triggered",
  "hook_failed",
  "error",
])

export function deferPresentationForEvent(
  event: { readonly type: EngineEvent["type"] },
): boolean {
  return !IMMEDIATE_PRESENTATION_EVENTS.has(event.type)
}

/** Build the retained OpenTUI application tree. */
export function createRottweilerApp(
  renderer: RenderContext,
  options: RottweilerAppOptions,
): RottweilerApp {
  return new RottweilerApp(renderer, options)
}
