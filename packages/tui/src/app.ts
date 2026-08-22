import {
  BoxRenderable,
  CliRenderEvents,
  SelectRenderableEvents,
  fg,
  t,
  type KeyEvent,
  type RenderContext,
  type Selection,
  type ThemeMode,
  type TreeSitterClient,
} from "@opentui/core"
import { homedir } from "node:os"

import {
  ComposerRenderable,
  ContextPanelRenderable,
  FuzzyPickerRenderable,
  InteractionPanelRenderable,
  OutputViewerRenderable,
  ReviewPanelRenderable,
  StateBannerRenderable,
  StatusLineRenderable,
  SubagentTrayRenderable,
  TranscriptRenderable,
  formatSubagentElapsed,
  type PickerItem,
} from "./components"
import {
  compileKeybindings,
  formatKeycap,
  keyStrokeFromEvent,
  legacyMacNavigationAction,
  KEYBINDING_ACTION_LABELS,
  type CompiledKeybindings,
  type InputMode,
  type KeybindingAction,
  type KeybindingConfiguration,
  type KeybindingContext,
  type KeybindingPreset,
  type VimFocus,
} from "./keybindings"
import {
  noExternalEditor,
  noExternalUrl,
  noImagePaste,
  noNotifications,
  noTextClipboard,
  type EditorAdapter,
  type ExternalUrlAdapter,
  type ImagePasteAdapter,
  type NotificationAdapter,
  type TextClipboardAdapter,
} from "./platform"
import {
  type ApprovalDecision,
  type ApprovalBinding,
  type Attachment,
  type ClientCommand,
  type CommandOutcome,
  type EngineEvent,
  type PlanDecision,
  type ModeId,
  type PermissionAction,
  type PermissionApprovalScope,
} from "./protocol"
import {
  ProjectionRequestBroker,
  type ProjectionKind,
} from "./projection-requests"
import {
  PresentationController,
  type PresentationFrameScheduler,
} from "./presentation"
import { PickerController, type PickerKind } from "./picker-controller"
import { presentError, sanitizeErrorFragment } from "./render"
import { setWorkspaceRoots } from "./render/tool-presentation"
import {
  commandSourceLabel,
  isLocalSlashCommand,
  isU64,
  mergeSlashCommandChoices,
  parseSessionAction,
  type CommandChoice,
} from "./session-commands"
import {
  createInitialState,
  enterReplayMode,
  engineEvent,
  projectSessionTitleUpdate,
  reduceRottweilerState,
  type QuestionProjection,
  type RottweilerState,
  type ToolProjection,
} from "./state"
import {
  createSubagentReplayState,
  transitionSubagentReplay,
  type SubagentReplayEffect,
  type SubagentReplayInput,
  type SubagentReplayState,
} from "./subagent-replay"
import {
  boundSubagentState,
  childEngineEvent,
  childPassiveInteractionState,
  initialSubagentState,
  mergeComposerDraft,
  sanitizeSubagentDescriptor,
  wireEventBytes,
  type ComposerDraft,
  type SubagentDescriptor,
} from "./subagent-state"
import {
  createSyntaxStyle,
  kennelTheme,
  systemThemeFor,
  themeByName,
  themeCatalog,
  type RottweilerTheme,
} from "./theme"
import {
  durableSequenceId,
  isRecord,
  isSessionForkedEvent,
  type WireEngineEvent,
} from "./transport"
import { stabilizeTreeSitterClient } from "./tree-sitter-client"
import {
  boundedUiText,
  contextPanelHasContent,
  mcpStateLabel,
  mcpTransportLabel,
  modePickerPresentation,
  modelAliasDescription,
  modelAvailabilityLabel,
  permissionActionLabel,
  permissionPatternLabel,
  permissionRuleActionLabel,
  providerConnectionStatus,
  providerDisplayName,
  providerName,
  providerStatusDetail,
  queuedMessageLabel,
  nextModeId,
  timelineTurnLabel,
} from "./ui-presentation"

export interface RottweilerAppOptions {
  readonly initialEvent?: EngineEvent
  readonly initialState?: RottweilerState
  readonly sessionId?: string
  readonly clientId?: string
  readonly onCommand?: (
    command: ClientCommand,
  ) => void | CommandOutcome | null | Promise<void | CommandOutcome | null>
  readonly onProviderApiKey?: (
    provider: string,
    apiKey: string
  ) => Promise<{
    readonly stored: true
    readonly activated: boolean
    readonly warnings: readonly string[]
  }>
  readonly onProviderActivate?: (provider: string) => Promise<void>
  readonly requestId?: () => string
  readonly theme?: RottweilerTheme
  readonly systemThemeMode?: ThemeMode | null
  readonly systemTheme?: RottweilerTheme
  readonly treeSitterClient?: TreeSitterClient
  readonly notifications?: NotificationAdapter
  readonly editor?: EditorAdapter
  readonly imagePaste?: ImagePasteAdapter
  readonly externalUrl?: ExternalUrlAdapter
  readonly textClipboard?: TextClipboardAdapter
  readonly terminalHandover?: TerminalHandoverAdapter
  readonly onSessionSelect?: (sessionId: string) => void | Promise<void>
  /** Close the complete supervised application. The supervisor reaps its owned engine. */
  readonly onExit?: () => void
  /** Historical presentation is observer-only; the composer and mutating interactions are hidden. */
  readonly replaySessionId?: string
  /** TUI-local bindings. Standard is backward-compatible; Vim enables modal editing/navigation. */
  readonly keybindings?: KeybindingConfiguration
  /** Injectable frame scheduler used to coalesce presentation-only stream deltas. */
  readonly presentationFrame?: PresentationFrameScheduler
  /** Startup instrumentation invoked only after the composer accepts changed input. */
  readonly onComposerInput?: (value: string) => void
  /** Host platform used for terminal compatibility decoding. Injectable for production-path tests. */
  readonly platform?: NodeJS.Platform
}

export type { PresentationFrameScheduler } from "./presentation"

interface PendingPresentationEvent {
  readonly event: WireEngineEvent
  readonly eventRecord: Record<string, unknown>
  readonly commandRequestId: string | null
  readonly previous: RottweilerState
  readonly next: RottweilerState
}

export interface TerminalHandoverAdapter {
  suspend(): void
  resume(): void
}

export interface TuiRecycleState {
  readonly schemaVersion: 1
  readonly draft: string
  readonly scrollTop: number
}

type BudgetSettingKey =
  | "budget.session_cost_cap_micros_usd"
  | "budget.daily_cost_cap_micros_usd"
  | "budget.warn_at_percent"
const MAX_VISIBLE_SUBAGENTS = 256
const EMPTY_TEXT_PROMPT_SENTINEL = "\u200B"

interface TimelineTurnChoice {
  readonly sequenceId: string
  readonly agentTurn: string
  readonly rewindTarget: string
  readonly content: string
  readonly hadAttachments: boolean
}

type TimelineAction = "edit" | "retry" | "rewind"

interface PendingRewindIntent {
  readonly action: TimelineAction
  readonly target: string
  readonly content: string
  readonly hadAttachments: boolean
  requestId: string | null
}

type SubagentAction =
  | { readonly kind: "inspect"; readonly subagent: SubagentDescriptor }
  | { readonly kind: "continue"; readonly subagent: SubagentDescriptor }
  | { readonly kind: "running"; readonly subagent: SubagentDescriptor }
  | { readonly kind: "interrupt"; readonly subagent: SubagentDescriptor }
  | { readonly kind: "close"; readonly subagent: SubagentDescriptor }
type ModelPickerChoice =
  | { readonly kind: "alias"; readonly alias: RottweilerState["modelAliases"][number] }
  | { readonly kind: "model"; readonly model: RottweilerState["models"][number] }

type PermissionPickerAction =
  | { readonly kind: "refresh" }
  | { readonly kind: "mode"; readonly mode: PermissionMode }
  | { readonly kind: "add"; readonly action: PermissionAction }
  | { readonly kind: "remove"; readonly ruleId: string }
  | { readonly kind: "revoke"; readonly approvalId: string; readonly scope: PermissionApprovalScope }
  | { readonly kind: "info" }
type QueuedMessagePickerAction =
  | { readonly kind: "remove"; readonly position: string }
  | { readonly kind: "clear" }
type SessionProjection = RottweilerState["sessions"][number]
type SessionPickerAction =
  | { readonly kind: "resume"; readonly session: SessionProjection }
  | { readonly kind: "rename"; readonly session: SessionProjection }
type ExportFormat = "markdown" | "html" | "json"
interface PendingExport {
  readonly format: ExportFormat
  readonly outputPath: string
  readonly force: boolean
  readonly requestId: string | null
}
type PermissionModePickerAction = Extract<PermissionPickerAction, { readonly kind: "mode" }>

interface PaletteAction {
  readonly id: string
  readonly title: string
  readonly description: string
  readonly section: PaletteSection
  readonly run: () => void
}

type PaletteSection =
  | "Conversation"
  | "Agents & models"
  | "Workspace"
  | "Safety"
  | "Appearance & settings"
  | "Help & system"
  | "Commands"

type PermissionMode = "strict" | "auto-safe" | "yolo" | "default"

interface PermissionModeChoice {
  readonly mode: PermissionMode
  readonly description: string
}

const PALETTE_SECTIONS: readonly PaletteSection[] = [
  "Conversation",
  "Agents & models",
  "Workspace",
  "Safety",
  "Appearance & settings",
  "Help & system",
  "Commands",
]

const KEYBOARD_HELP_CONTEXT_NAMES: Record<KeybindingContext, string> = {
  global: "Global",
  standard: "Editing",
  vim_normal: "Normal mode",
  vim_insert: "Insert mode",
  picker_normal: "Picker normal mode",
  picker_insert: "Picker insert mode",
  review: "Review",
}

const KEYBOARD_HELP_CONTEXTS: Record<KeybindingPreset, readonly KeybindingContext[]> = {
  standard: ["global", "standard", "review"],
  vim: ["global", "vim_normal", "vim_insert", "picker_normal", "picker_insert", "review"],
}

const PERMISSION_MODE_CHOICES: readonly PermissionModeChoice[] = [
  { mode: "strict", description: "Ask before every tool use" },
  { mode: "auto-safe", description: "Ask only for risky actions" },
  { mode: "yolo", description: "Never ask · dangerous" },
  { mode: "default", description: "Follow the launch policy" },
]

const EXPORT_FORMAT_CHOICES: readonly {
  readonly format: ExportFormat
  readonly label: string
  readonly description: string
  readonly extension: string
}[] = [
  { format: "markdown", label: "Markdown", description: "Readable text", extension: "md" },
  { format: "html", label: "HTML", description: "Formatted for a browser", extension: "html" },
  { format: "json", label: "JSON", description: "Structured data", extension: "json" },
]

type ProviderAuthPickerAction =
  | { readonly kind: "open_url"; readonly value: string }
  | { readonly kind: "copy_url"; readonly value: string }
  | { readonly kind: "copy_code"; readonly value: string }
  | { readonly kind: "cancel" }

type McpPickerAction =
  | { readonly kind: "actions"; readonly server: string }
  | { readonly kind: "add_http" }
  | { readonly kind: "add_stdio" }
  | { readonly kind: "retry" }
type McpServerAction =
  | { readonly kind: "toggle"; readonly server: string; readonly enabled: boolean }
  | { readonly kind: "review"; readonly server: string }
  | { readonly kind: "approve"; readonly server: string; readonly fingerprint: string }
  | { readonly kind: "remove"; readonly server: string }

export class RottweilerApp extends BoxRenderable {
  transcript!: TranscriptRenderable
  contextPanel!: ContextPanelRenderable
  interactionPanel!: InteractionPanelRenderable
  outputViewer!: OutputViewerRenderable
  reviewPanel!: ReviewPanelRenderable
  picker!: FuzzyPickerRenderable<unknown>
  composer!: ComposerRenderable
  statusLine!: StatusLineRenderable
  subagentTray!: SubagentTrayRenderable
  banner!: StateBannerRenderable
  main!: BoxRenderable

  #state: RottweilerState
  #workspaceRoots: RottweilerState["workspaceRoots"] | undefined
  #options: Required<
    Pick<
      RottweilerAppOptions,
      | "sessionId"
      | "clientId"
      | "requestId"
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
  #themeBeforePreview: RottweilerTheme | null = null
  #themePreviewCommitted = false
  #rethemeInProgress = false
  #composerSubmissionsInFlight = 0
  #deferredTheme: RottweilerTheme | null = null
  #sessionId: string
  #terminalFocused = true
  #systemThemeMode: ThemeMode | null
  #systemTheme: RottweilerTheme
  #projectionRequests: ProjectionRequestBroker
  #pickerController: PickerController
  #subagentListError: string | null = null
  #subagentReplays = new Map<string, SubagentReplayState<WireEngineEvent>>()
  #subagentDescriptors: readonly SubagentDescriptor[] = []
  #subagentStates = new Map<string, RottweilerState>()
  #activeSubagentReadOnly = false
  #parentComposerDraft: ComposerDraft = { content: "", attachments: [] }
  #subagentComposerDrafts = new Map<string, ComposerDraft>()
  #activeSubagentId: string | null = null
  #subagentActionId: string | null = null
  #interruptSubagentId: string | null = null
  #subagentErrorBaseline: RottweilerState["errors"][number] | undefined
  #commandsRequested = false
  #commandCatalogTruncationNotified = false
  #modelsRequested = false
  #providerOnboardingOffered = false
  #providerOnboardingModelsResponseReceived = false
  #providerOnboardingSessionsResponseReceived = false
  #providerPickerOnboarding = false
  #projectionErrors: Partial<Record<ProjectionKind, string>> = {}
  #modelProviderFilter: string | null = null
  #providerApiKeyProvider: string | null = null
  #providerRecoveryProvider: RottweilerState["providers"][number] | null = null
  #providerAuthActionInFlight = false
  #providerAuthActionNotice: string | null = null
  #providerAuthCompletionAttempts = new Set<string>()
  #storedProviderKeys = new Set<string>()
  #providerApiKeyPending: string | null = null
  #mcpDraftName: string | null = null
  #mcpDraftExecutable: string | null = null
  #mcpDraftArgs: string[] = []
  #mcpDraftEnvironment: Array<{ readonly key: string; readonly value: string }> = []
  #mcpActionName: string | null = null
  #budgetSettingKey: BudgetSettingKey | null = null
  #outputViewerToolCallId: string | null = null
  #reviewOpen = false
  #pendingReviewSelection: string | null = null
  #postSubmitPicker: "models" | "providers" | "themes" | "settings" | "permissions" | "mcp" | "agents" | null = null
  #terminalSuspended = false
  #pendingShellTimer: ReturnType<typeof setTimeout> | null = null
  #pluginNotificationTimer: ReturnType<typeof setTimeout> | null = null
  #sessionSearchTimer: ReturnType<typeof setTimeout> | null = null
  #runtimeServicesTimer: ReturnType<typeof setTimeout> | null = null
  #interruptEscapeTimer: ReturnType<typeof setTimeout> | null = null
  #clipboardNoticeTimer: ReturnType<typeof setTimeout> | null = null
  #exportNoticeTimer: ReturnType<typeof setTimeout> | null = null
  #interruptEscapeArmed = false
  #pendingReviewPaths = new Set<string>()
  #sessionActionId: string | null = null
  #timelineTurn: TimelineTurnChoice | null = null
  #pendingRewindIntent: PendingRewindIntent | null = null
  #composerNotice: string | null = null
  #pendingExport: PendingExport | null = null
  #lastComposerValue = ""
  #pendingRecycleScrollTop: number | null = null
  #keybindings: CompiledKeybindings
  #inputMode: InputMode
  #vimFocus: VimFocus = "composer"
  #vimFocusBeforePicker: Exclude<VimFocus, "picker"> = "composer"
  #destroyed = false
  #presentation: PresentationController<PendingPresentationEvent>
  #onTerminalFocus = () => {
    this.#terminalFocused = true
  }
  #onTerminalBlur = () => {
    this.#terminalFocused = false
    this.#clearInterruptEscape()
  }
  #onTerminalThemeMode = (mode: ThemeMode) => {
    this.#systemThemeMode = mode
    if (this.#theme.name !== "system") {
      const next = themeByName(this.#theme.name, mode)
      if (next !== undefined && next.background !== this.#theme.background) this.#createThemedSurface(next)
      return
    }
    // Palette refresh is owned by production startup. If the terminal changes
    // mode during a session, immediately switch to a safe matching fallback
    // rather than retaining an unreadable stale palette.
    this.#systemTheme = systemThemeFor(mode)
    const next = this.#systemTheme
    if (next.background !== this.#theme.background) this.#createThemedSurface(next)
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
        if (!this.#state.replay.active) this.#focusForInputMode()
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
  #onGlobalKey = (key: KeyEvent) => {
    const focusOwner = this.#visibleFocusOwner()
    const plainEscape = keyStrokeFromEvent(key) === "escape"
    if (!plainEscape && this.#interruptEscapeArmed) this.#clearInterruptEscape()
    if (
      plainEscape &&
      this.#activeSubagentId !== null &&
      !this.picker.visible &&
      this.#outputViewerToolCallId === null &&
      !this.#reviewOpen
    ) {
      if (this.#keybindings.preset === "vim" && this.#inputMode === "insert") {
        this.#setInputMode("normal")
        key.preventDefault()
        key.stopPropagation()
        return
      }
      const subagentId = this.#activeSubagentId
      const running = this.#subagentDescriptor(subagentId)?.activity === "running"
      this.#leaveSubagent()
      if (running) this.#armInterruptEscape(subagentId)
      key.preventDefault()
      key.stopPropagation()
      return
    }
    if (
      plainEscape &&
      !this.picker.visible &&
      this.#outputViewerToolCallId === null &&
      !this.#reviewOpen &&
      this.#isInterruptible()
    ) {
      // In Vim mode the first Escape still leaves insert mode, but it also
      // counts as the first half of the universal double-Escape interrupt.
      if (this.#inputMode === "insert") this.#setInputMode("normal")
      if (this.#interruptEscapeArmed) {
        const subagentId = this.#interruptSubagentId
        this.#clearInterruptEscape()
        void this.#interruptActiveResponse(subagentId)
      } else {
        this.#armInterruptEscape()
      }
      key.preventDefault()
      key.stopPropagation()
      return
    }
    if (
      focusOwner === "composer" &&
      !this.picker.visible &&
      this.#outputViewerToolCallId === null &&
      !this.#reviewOpen &&
      !key.ctrl &&
      !key.meta &&
      !key.super &&
      !key.option &&
      !key.hyper &&
      !key.shift &&
      (key.name === "up" || key.name === "down") &&
      this.composer.navigateHistory(key.name === "up" ? "previous" : "next")
    ) {
      if (this.#pickerController.anchored) this.closePicker()
      key.preventDefault()
      key.stopPropagation()
      return
    }
    if (
      focusOwner === "interaction" &&
      !key.ctrl &&
      !key.meta &&
      !key.super &&
      !key.option &&
      !key.hyper &&
      !key.shift &&
      (key.name === "return" || key.name === "kpenter" || key.name === "linefeed")
    ) {
      // SelectRenderable handles Return internally but does not normalize the
      // keypad Enter or raw line-feed event on every terminal. Own all shapes
      // at the global priority layer so the focused safety choice is committed
      // exactly once.
      this.interactionPanel.select.selectCurrent()
      key.preventDefault()
      key.stopPropagation()
      return
    }
    const legacyMacNavigation = focusOwner === "composer"
      ? legacyMacNavigationAction(key, this.#options.platform ?? process.platform)
      : null
    if (
      legacyMacNavigation !== null &&
      this.#handleKeybindingAction(legacyMacNavigation)
    ) {
      key.preventDefault()
      key.stopPropagation()
      return
    }
    if (
      focusOwner === "composer" &&
      !this.picker.visible &&
      key.name === "backspace" &&
      !key.ctrl && !key.meta && !key.option &&
      this.composer.value.length === 0 &&
      this.composer.removeLastAttachment()
    ) {
      key.preventDefault()
      key.stopPropagation()
      return
    }
    const safetyPanelFocused =
      focusOwner === "interaction" || focusOwner === "output" || focusOwner === "review"
    const action =
      focusOwner === "output" || focusOwner === "review"
        ? this.#keybindings.resolve("review", key)
        : focusOwner === "interaction"
          ? null
          : ( this.#keybindings.resolve("global", key) ??
            this.#keybindings.resolve(this.#keybindingContext(), key))
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
      externalUrl: options.externalUrl ?? noExternalUrl,
      textClipboard: options.textClipboard ?? noTextClipboard,
    }
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
    this.#keybindings = compileKeybindings(options.keybindings)
    this.#inputMode = this.#keybindings.preset === "vim" ? "normal" : "standard"
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
    if (this.#state.replay.active && this.#keybindings.preset === "vim") {
      this.#vimFocus = "transcript"
      this.#vimFocusBeforePicker = "transcript"
    }
    if (options.initialEvent !== undefined) {
      this.#state = reduceRottweilerState(this.#state, engineEvent(options.initialEvent))
    }
    this.#presentation = new PresentationController({
      scheduler: options.presentationFrame,
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
      renderPicker: () => this.#renderPicker(),
      withRefreshGuard: (kind, refresh) => {
        const suppressThemePreview = kind === "themes" || kind === "settings"
        const rethemeWasInProgress = this.#rethemeInProgress
        if (suppressThemePreview) this.#rethemeInProgress = true
        try {
          refresh()
        } finally {
          this.#rethemeInProgress = rethemeWasInProgress
        }
      },
      onModalOpened: () => {
        if (this.#keybindings.preset !== "vim" || this.#vimFocus === "picker") return
        this.#vimFocusBeforePicker = this.#vimFocus
        this.#vimFocus = "picker"
        this.#setInputMode("insert")
      },
      onClosed: (kind) => this.#afterPickerClosed(kind),
    })

    this.#createThemedSurface(theme)
    ctx.on(CliRenderEvents.FOCUS, this.#onTerminalFocus)
    ctx.on(CliRenderEvents.BLUR, this.#onTerminalBlur)
    ctx.on(CliRenderEvents.THEME_MODE, this.#onTerminalThemeMode)
    ctx.on(CliRenderEvents.SELECTION, this.#onSelection)
    ctx.keyInput.on("keypress", this.#onGlobalKey)
    this.setState(this.#state)
    if (this.#reviewOpen) {
      this.reviewPanel.files.focus()
    } else if (!this.#state.replay.active) {
      this.#focusForInputMode()
    }
  }

  #createThemedSurface(theme: RottweilerTheme): void {
    const rebuilding = this.getChildrenCount() > 0
    if (rebuilding && this.#composerSubmissionsInFlight > 0) {
      this.#deferredTheme = theme
      return
    }
    this.#deferredTheme = null
    const draft = rebuilding ? this.composer.value : ""
    const attachments = rebuilding ? [...this.composer.attachments] : []
    const scrollTop = rebuilding ? this.transcript.scroller.scrollTop : 0
    const pickerWasVisible = rebuilding && this.picker.visible
    const pickerKind = this.#pickerController.kind
    const pickerQuery = rebuilding ? this.picker.input.value : ""
    const pickerSelection = rebuilding
      ? this.picker.select.getSelectedOption()?.value
      : undefined
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
      gap: 1,
    })
    this.transcript = new TranscriptRenderable(this.ctx, theme, {
      syntaxStyle: this.#syntaxStyle,
      ...(this.#treeSitterClient === undefined
        ? {}
        : { treeSitterClient: this.#treeSitterClient }),
      onInteraction: () => this.#restoreFocusAfterTranscriptInteraction(),
      onOpenSubagent: (subagentId) => {
        void this.#enterSubagent(subagentId)
      },
      onOpenToolOutput: (toolCallId) => this.#openToolOutput(toolCallId),
    })
    this.contextPanel = new ContextPanelRenderable(this.ctx, theme, {
      onOpenDiff: (path) => this.#openChangedFileDiff(path),
    })
    this.main.add(this.transcript)
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
      (subagentId) => void this.#enterSubagent(subagentId),
      () => {
        if (this.#activeSubagentId !== null) this.#updateSubagentBanner(this.#presentedState())
      },
    )
    const picker = new FuzzyPickerRenderable(this.ctx, theme, (query) => {
      if (this.picker !== picker) return
      if (this.#pickerController.kind === "sessions") this.#scheduleSessionSearch(query)
    })
    this.picker = picker
    picker.select.on(SelectRenderableEvents.SELECTION_CHANGED, () => {
      // A preview rebuild destroys the old picker. Ignore any queued selection
      // notification from that generation instead of reading its dead buffer.
      if (this.picker !== picker) return
      if ((this.#pickerController.kind !== "themes" && this.#pickerController.kind !== "settings") || this.#rethemeInProgress) return
      const id = picker.select.getSelectedOption()?.value
      if (typeof id !== "string") return
      const name = id.startsWith("theme:")
        ? id.slice("theme:".length)
        : id.startsWith("ui.theme:")
          ? id.slice("ui.theme:".length)
          : null
      if (name === null) return
      const selected = name === "system"
        ? this.#systemTheme
        : themeByName(name, this.#systemThemeMode ?? "dark")
      if (selected !== undefined) {
        if (this.#themeBeforePreview === null) this.#themeBeforePreview = this.#theme
        this.#previewTheme(selected)
      }
    })
    this.picker.position = "absolute"
    this.picker.top = 2
    this.picker.left = "15%"
    this.picker.width = "70%"
    const pasteImageKeycap = this.#bindingHint("paste_image", ["global", this.#composerKeybindingContext()])
    const externalEditorKeycap = this.#bindingHint("open_external_editor", ["global", this.#composerKeybindingContext()])
    this.composer = new ComposerRenderable(this.ctx, theme, {
      editor: this.#options.editor,
      imagePaste: this.#options.imagePaste,
      ...(pasteImageKeycap === null ? {} : { pasteImageKeycap }),
      ...(externalEditorKeycap === null ? {} : { externalEditorKeycap }),
      onSubmit: async (content, submittedAttachments) => {
        this.#composerSubmissionsInFlight += 1
        return await this.#sendMessage(content, submittedAttachments)
      },
      submissionScope: () => this.#composerScope(),
      onDetachedSubmissionRejected: (scope, content, attachments) =>
        this.#restoreDetachedSubmission(scope, content, attachments),
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
      },
    })
    this.statusLine = new StatusLineRenderable(this.ctx, theme, {
      modelPickerKeycap: this.#bindingHint("open_model_picker", ["global"]),
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
    this.setState(this.#state)
    this.composer.value = draft
    for (const attachment of attachments) this.composer.addAttachment(attachment)
    this.transcript.setScrollOffset(scrollTop)

    if (pickerWasVisible && pickerKind !== null) {
      this.#pickerController.refresh()
      const selectedIndex = this.picker.select.options.findIndex(
        (option) => option.value === pickerSelection,
      )
      if (selectedIndex >= 0) this.picker.select.setSelectedIndex(selectedIndex)
      this.picker.input.value = pickerQuery
    }
    this.#rethemeInProgress = false
    if (this.picker.visible && !this.#pickerController.anchored) this.picker.input.focus()
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

  get activeSubagentId(): string | null {
    return this.#activeSubagentId
  }

  /** Presentation state is exposed for focused UI tests; parent state remains `state`. */
  get visibleState(): RottweilerState {
    return this.#presentedState()
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
      this.#projectionRequests.clearForSessionChange()
      this.#subagentListError = null
      this.#subagentReplays.clear()
      this.#subagentDescriptors = []
      this.#subagentStates.clear()
      this.#parentComposerDraft = { content: "", attachments: [] }
      this.#subagentComposerDrafts.clear()
      this.#activeSubagentId = null
      this.#subagentActionId = null
      this.#subagentErrorBaseline = undefined
      this.#commandsRequested = false
      this.#commandCatalogTruncationNotified = false
      this.#modelsRequested = false
      this.#projectionErrors = {}
      this.#pendingReviewSelection = null
      this.#outputViewerToolCallId = null
      this.#reviewOpen = false
      this.#timelineTurn = null
      this.#pendingRewindIntent = null
      this.#composerNotice = null
      this.#pendingExport = null
      this.#clearExportNotice()
      this.#providerApiKeyProvider = null
      this.#providerRecoveryProvider = null
      this.#storedProviderKeys.clear()
      this.#providerAuthActionInFlight = false
      this.#providerAuthActionNotice = null
      this.#providerAuthCompletionAttempts.clear()
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
    this.#commandsRequested = false
    this.#modelsRequested = false
    this.#projectionErrors = {}
  }

  handleEvent(event: WireEngineEvent): void {
    if (this.#destroyed) return
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
      this.#subagentListError = null
      this.#subagentDescriptors = listed.subagents
        .slice(0, MAX_VISIBLE_SUBAGENTS)
        .map(sanitizeSubagentDescriptor)
        .filter((descriptor): descriptor is SubagentDescriptor => descriptor !== null)
      if (
        this.#activeSubagentId !== null &&
        this.#subagentDescriptor(this.#activeSubagentId) === undefined
      ) this.#leaveSubagent()
      else this.setState(this.#state)
      return
    }
    if (event.type === "subagent_replay_batch") {
      const replay = event as Extract<EngineEvent, { type: "subagent_replay_batch" }>
      if (replay.session_id !== this.#sessionId) return
      if (commandRequestId === null) return
      const events = replay.events.flatMap((item) => {
        const childEvent = childEngineEvent(item.event, replay.child_session_id)
        return childEvent === null
          ? []
          : [{
              sequence: item.child_sequence,
              eventSequence: durableSequenceId(childEvent),
              event: childEvent,
            }]
      })
      this.#transitionSubagentReplay(replay.subagent_id, {
        type: "replayBatch",
        requestId: commandRequestId,
        childSessionId: replay.child_session_id,
        events,
      })
      return
    }
    if (event.type === "subagent_replay_completed") {
      const completed = event as Extract<EngineEvent, { type: "subagent_replay_completed" }>
      if (completed.session_id !== this.#sessionId) return
      if (commandRequestId === null) return
      const descriptor = this.#subagentDescriptor(completed.subagent_id)
      if (descriptor === undefined) return
      this.#transitionSubagentReplay(completed.subagent_id, {
        type: "replayCompleted",
        requestId: commandRequestId,
        childSessionId: descriptor.child_session_id,
        throughSequence: completed.through_sequence ?? null,
        nextCursor: completed.next_cursor ?? null,
        tailSequence: completed.tail_sequence ?? null,
        hasMore: completed.has_more,
        eventsBeforePage: completed.events_before_page,
        truncated: completed.truncated,
      })
      if (this.#activeSubagentId === completed.subagent_id) this.setState(this.#state)
      return
    }
    if (event.type === "subagent_progress") {
      const progress = event as Extract<EngineEvent, { type: "subagent_progress" }>
      if (progress.parent_session_id !== this.#sessionId) return
      const descriptor = this.#subagentDescriptor(progress.subagent_id)
      if (descriptor === undefined || descriptor.child_session_id !== progress.child_session_id) return
      const childEvent = childEngineEvent(progress.event, progress.child_session_id)
      if (childEvent !== null) {
        this.#transitionSubagentReplay(progress.subagent_id, {
          type: "liveProgress",
          childSessionId: progress.child_session_id,
          childSequence: progress.child_sequence ?? null,
          eventSequence: durableSequenceId(childEvent),
          event: childEvent,
          bytes: wireEventBytes(progress),
        })
      }
      const existing = this.#state.subagents[progress.subagent_id]
      if (existing === undefined || existing.childSessionId !== progress.child_session_id) return
    }
    if (event.type === "session_forked") {
      if (
        !isSessionForkedEvent(event) ||
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
    if (completedProjection === "commands") this.#commandsRequested = false
    if (completedProjection === "models") this.#modelsRequested = false
    if (completedProjection !== null) this.#clearProjectionError(completedProjection)
    const previous = this.#state
    const crossSessionTitle =
      event.type === "session_title_updated" &&
      isRecord(event.meta) &&
      event.meta.session_id !== this.#sessionId
    let next =
      crossSessionTitle
        ? projectSessionTitleUpdate(
            previous,
            event as Extract<EngineEvent, { type: "session_title_updated" }>,
          )
        : reduceRottweilerState(previous, engineEvent(event))
    if (event.type === "sessions_listed" && Array.isArray(eventRecord.sessions)) {
      const active = eventRecord.sessions.find(
        (session) => isRecord(session) && session.session_id === this.#sessionId,
      )
      if (isRecord(active) && typeof active.model === "string") {
        const separator = active.model.indexOf("/")
        next = {
          ...next,
          model: active.model,
          provider: separator > 0 ? active.model.slice(0, separator) : null,
        }
      }
    }
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
    const pendingRewind = this.#pendingRewindIntent
    const causedBy = isRecord(eventRecord.meta) && typeof eventRecord.meta.caused_by === "string"
      ? eventRecord.meta.caused_by
      : null
    const rewindAckOutcome = commandRequestId === null
      ? null
      : next.commandAcks[commandRequestId]?.outcome ?? null
    if (
      pendingRewind !== null &&
      event.type === "command_acknowledged" &&
      commandRequestId === pendingRewind.requestId &&
      rewindAckOutcome?.type === "rejected"
    ) {
      this.#pendingRewindIntent = null
      this.#projectRejection(rewindAckOutcome)
    } else if (
      pendingRewind !== null &&
      event.type === "error" &&
      (causedBy === null || causedBy === pendingRewind.requestId)
    ) {
      this.#pendingRewindIntent = null
    } else if (
      pendingRewind !== null &&
      event.type === "conversation_rewound" &&
      event.to_agent_turn === pendingRewind.target &&
      (causedBy === null || causedBy === pendingRewind.requestId)
    ) {
      // Clear before applying the follow-up so a duplicate durable event cannot fire it twice.
      this.#pendingRewindIntent = null
      if (pendingRewind.action === "edit") {
        this.composer.value = pendingRewind.content
        this.#composerNotice = pendingRewind.hadAttachments
          ? "attachments from the original message are not restored"
          : null
        this.composer.focus()
        this.setState(this.#state)
      } else if (pendingRewind.action === "retry") {
        void this.#sendMessage(pendingRewind.content, [])
      }
    }
    const modelSwitchOutcome =
      event.type === "command_acknowledged" &&
      commandRequestId !== null &&
      this.#projectionRequests.consumeModelSwitch(commandRequestId)
        ? next.commandAcks[commandRequestId]?.outcome
        : null
    if (modelSwitchOutcome?.type === "rejected") {
      this.#projectRejection(modelSwitchOutcome)
    }
    const pendingExport = this.#pendingExport
    if (
      event.type === "command_acknowledged" &&
      pendingExport !== null &&
      pendingExport.requestId !== null &&
      commandRequestId === pendingExport.requestId &&
      (event as Extract<EngineEvent, { type: "command_acknowledged" }>).outcome.type === "rejected"
    ) {
      this.#handleExportRejection(
        (event as Extract<EngineEvent, { type: "command_acknowledged" }>).outcome as Extract<CommandOutcome, { type: "rejected" }>,
        pendingExport,
      )
    } else if (
      event.type === "session_exported" &&
      (event as Extract<EngineEvent, { type: "session_exported" }>).session_id === this.#sessionId &&
      pendingExport !== null &&
      pendingExport.requestId !== null &&
      commandRequestId === pendingExport.requestId
    ) {
      this.#pendingExport = null
      this.#showExportNotice(
        (event as Extract<EngineEvent, { type: "session_exported" }>).output_path,
      )
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
    if (isSessionForkedEvent(event)) {
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
      this.#requestCommands()
      this.#requestModes()
    }
    if (event.type === "subagent_spawned" || event.type === "subagent_finished") {
      this.#requestSubagents()
    }
    if (event.type === "models_listed") {
      const activationCatalog = this.#projectionRequests.consumeProviderActivationModels(
        commandRequestId,
      )
      if (
        activationCatalog &&
        !next.replay.active &&
        this.#activeSubagentId === null
      ) {
        const availableModels = next.models.filter((model) => model.available !== false)
        if (availableModels.length === 1) {
          const model = availableModels[0]!
          this.#projectionRequests.command({
            type: "switch_model",
            model: model.id ?? model.alias,
            provider: model.provider ?? null,
          })
          this.closePicker()
        }
      }
      if (
        !this.#providerOnboardingModelsResponseReceived &&
        next.connection.phase === "connected"
      ) {
        this.#providerOnboardingModelsResponseReceived = true
        this.#maybeOfferProviderOnboarding(next)
      }
    }
    if (
      event.type === "sessions_listed" &&
      !this.#providerOnboardingSessionsResponseReceived &&
      next.connection.phase === "connected"
    ) {
      this.#providerOnboardingSessionsResponseReceived = true
      this.#maybeOfferProviderOnboarding(next)
    }
    if (event.type === "provider_auth_started") {
      const provider = typeof eventRecord.provider === "string" ? eventRecord.provider : null
      const attemptId = typeof eventRecord.attempt_id === "string" ? eventRecord.attempt_id : null
      if (provider === null || attemptId === null) return
      this.#providerAuthActionInFlight = false
      this.#providerAuthActionNotice = null
      const firstDelivery = !this.#providerAuthCompletionAttempts.has(attemptId)
      if (firstDelivery) {
        if (this.#providerAuthCompletionAttempts.size >= 64) {
          const oldest = this.#providerAuthCompletionAttempts.values().next().value
          if (oldest !== undefined) this.#providerAuthCompletionAttempts.delete(oldest)
        }
        this.#providerAuthCompletionAttempts.add(attemptId)
        this.#projectionRequests.command({
          type: "complete_provider_auth",
          provider,
          attemptId,
        })
      }
      this.openProviderAuthPicker()
      if (firstDelivery) {
        const challenge = next.providerAuth.pending?.challenge
        const url = challenge?.kind === "oauth"
          ? challenge.authorization_url
          : challenge?.verification_uri
        if (url !== undefined) {
          void this.#runProviderAuthAction(provider, attemptId, { kind: "open_url", value: url })
        }
      }
    }
    if (event.type === "provider_configured") {
      const provider = typeof eventRecord.provider === "string" ? eventRecord.provider : null
      if (provider === null) return
      if (eventRecord.auth_kind === "oauth" || eventRecord.auth_kind === "device_flow") {
        this.#projectionRequests.command({ type: "begin_provider_auth", provider })
      } else if (eventRecord.auth_kind === "api_key") {
        this.openProviderApiKeyPrompt(provider)
      }
    }
    if (event.type === "provider_auth_finished") {
      this.#providerAuthActionInFlight = false
      if (eventRecord.success === true) {
        this.#providerAuthActionNotice = "Signed in. Connecting provider and loading models…"
      } else {
        this.#providerAuthActionNotice = null
        this.#projectClientError(
          "provider_auth_failed",
          typeof eventRecord.message === "string" ? eventRecord.message : "provider authentication failed",
          true,
        )
      }
    }
    if (event.type === "provider_activation_finished") {
      this.#providerAuthActionNotice = null
      const message = typeof eventRecord.message === "string"
        ? eventRecord.message
        : "provider connection did not become ready"
      if (eventRecord.success === true) {
        this.#requestModels(true)
        this.#projectionRequests.markProviderActivationModels()
      } else {
        this.#projectClientError("provider_activation_failed", message, true)
      }
      this.openProviderPicker()
    }
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

  #maybeOfferProviderOnboarding(state: RottweilerState): void {
    if (
      !this.#providerOnboardingModelsResponseReceived ||
      !this.#providerOnboardingSessionsResponseReceived
    ) return
    const ready = state.providers.some(
      (provider) => provider.configured && provider.authenticated && provider.reachable,
    )
    // Waiting for both projections closes their ordering gap, but durable turn replay is
    // independently timed; state.model remains the final guard if replay restores it first,
    // with accepted residual risk only when neither restoration path has populated it before
    // a transiently unready provider catalog is evaluated.
    if (
      !ready &&
      state.model === null &&
      !this.#providerOnboardingOffered &&
      !state.replay.active &&
      this.#activeSubagentId === null &&
      this.composer.value.length === 0 &&
      this.#pickerController.kind === null
    ) {
      this.#providerOnboardingOffered = true
      this.openProviderPicker(true)
    }
  }

  setState(state: RottweilerState): void {
    if (this.#destroyed) return
    this.#presentation.flushBeforeStateChange()
    this.#bindStateToComponents(state)
  }

  /** Snapshot process-local UI state before the supervisor recycles this renderer. */
  recycleState(): TuiRecycleState {
    return {
      schemaVersion: 1,
      draft: this.composer.value,
      scrollTop: Math.max(0, this.transcript.scroller.scrollTop),
    }
  }

  /** Restore the non-durable UI state that cannot be replayed from the engine. */
  restoreRecycleState(state: TuiRecycleState): void {
    this.composer.restoreDraft(state.draft, [])
    this.#parentComposerDraft = { content: state.draft, attachments: [] }
    this.#lastComposerValue = state.draft
    this.#pendingRecycleScrollTop = state.scrollTop
  }

  /** Apply a restored offset after OpenTUI has laid out the replayed transcript. */
  applyPendingRecycleScroll(): void {
    if (
      this.#pendingRecycleScrollTop === null ||
      (this.#pendingRecycleScrollTop > 0 && this.#presentedState().transcript.length === 0)
    ) return
    this.transcript.setScrollOffset(this.#pendingRecycleScrollTop)
    this.#pendingRecycleScrollTop = null
  }

  #bindStateToComponents(state: RottweilerState): void {
    const previousConnectionPhase = this.#state.connection.phase
    const previousFocusOwner = this.#visibleFocusOwner()
    if (state.workspaceRoots !== this.#workspaceRoots) {
      this.#workspaceRoots = state.workspaceRoots
      setWorkspaceRoots(state.workspaceRoots?.roots ?? [])
    }
    this.#state = state
    if (state.providerAuth.pending === null) {
      this.#providerAuthActionInFlight = false
      this.#providerAuthActionNotice = null
    }
    const presented = this.#presentedState()
    const viewingSubagent = this.#activeSubagentId !== null
    const childDescriptor = this.#activeSubagentId === null
      ? undefined
      : this.#subagentDescriptor(this.#activeSubagentId)
    this.transcript.update(
      presented,
      viewingSubagent ? childDescriptor?.agent || "Child agent" : "Rottweiler",
    )
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
      !viewingSubagent &&
      !state.replay.active &&
      contextPanelHasContent(state) &&
      (this.width === 0 ? this.ctx.width >= 100 : this.width >= 100)
    this.interactionPanel.update(viewingSubagent ? childPassiveInteractionState(presented) : state)
    this.reviewPanel.update(state, !viewingSubagent && this.#reviewOpen)
    this.composer.setQueuedMessages(viewingSubagent ? [] : state.queuedMessages)
    const subagentReadOnly = this.#isActiveSubagentRunning()
    const subagentBecameWritable = this.#activeSubagentReadOnly && !subagentReadOnly
    this.#activeSubagentReadOnly = subagentReadOnly
    const composerVisible =
      !state.replay.active &&
      this.#outputViewerToolCallId === null &&
      !this.#reviewOpen &&
      !subagentReadOnly &&
      (!this.interactionPanel.visible || this.interactionPanel.usesComposer)
    if (!composerVisible) this.composer.editor.blur()
    this.composer.visible = composerVisible
    this.interactionPanel.resizeForTerminal(
      this.height === 0 ? this.ctx.height : this.height,
      this.interactionPanel.usesComposer && composerVisible ? this.composer.dockHeight : 0,
    )
    const focusOwner = this.#visibleFocusOwner()
    if (
      (previousFocusOwner === "interaction" ||
        previousFocusOwner === "output" ||
        previousFocusOwner === "review") &&
      focusOwner !== "interaction" &&
      focusOwner !== "output" &&
      focusOwner !== "review"
    ) {
      this.#focusForInputMode()
    } else if (subagentBecameWritable) {
      this.#focusForInputMode()
    }
    this.statusLine.setBranch(viewingSubagent ? null : state.workspaceStatus?.branch ?? null)
    this.statusLine.setKeybindingMode(
      this.#inputMode === "standard" ? null : this.#inputMode,
      this.#inputMode === "standard" ? null : this.#statusFocusOwner(),
    )
    this.statusLine.update(presented)
    this.banner.update(presented)
    if (this.#composerNotice !== null && this.composer.visible) {
      this.banner.visible = true
      this.banner.fg = this.#theme.muted
      this.banner.content = this.#composerNotice
    }
    if (viewingSubagent) this.#updateSubagentBanner(presented)
    if (!this.#isInterruptible()) this.#clearInterruptEscape(false)
    if (this.#interruptEscapeArmed) {
      this.banner.visible = true
      this.banner.fg = this.#theme.warning
      this.banner.content = this.#interruptSubagentId === null
        ? "Press Esc again to stop the active response"
        : "Back in parent · press Esc again to stop the child agent"
    }
    if (this.#pickerController.kind !== null) {
      this.#pickerController.refresh()
    }
    if (
      (state.connection.phase === "reconnecting" || state.connection.phase === "disconnected") &&
      state.connection.phase !== previousConnectionPhase
    ) this.#markSubagentReplayTransportLost()
    else if (
      state.connection.phase === "connected" &&
      (previousConnectionPhase === "reconnecting" || previousConnectionPhase === "disconnected")
    ) this.#recoverSubagentReplays()
  }

  openCommandPicker(): void {
    this.#pickerController.begin("palette")
    if (!this.#commandsRequested) {
      this.#requestCommands()
    }
    this.#pickerController.refresh()
  }

  openKeyboardHelpPicker(): void {
    this.#pickerController.begin("keyboardHelp")
    this.#pickerController.refresh()
  }

  openFilePicker(query = "", anchored = false): void {
    this.#pickerController.begin("files", anchored, query)
    this.#projectionRequests.command({ type: "search_workspace_files", query, limit: 100 })
    this.#pickerController.refresh()
    if (!anchored) this.picker.input.value = query
  }

  openAttachmentPicker(): void {
    this.#pickerController.begin("attachments")
    this.#pickerController.refresh()
  }

  openModelPicker(provider: string | null = null): void {
    this.#modelProviderFilter = provider
    this.#pickerController.begin("models")
    if (!this.#modelsRequested) {
      this.#requestModels(true)
    }
    this.#projectionRequests.command({ type: "list_settings" })
    this.#pickerController.refresh()
  }

  openProviderPicker(onboarding = false): void {
    this.#modelProviderFilter = null
    this.#providerPickerOnboarding = onboarding
    this.#pickerController.begin("providers")
    if (!this.#modelsRequested) {
      this.#requestModels(true)
    }
    this.#pickerController.refresh()
  }

  openProviderAuthPicker(): void {
    this.#pickerController.begin("providerAuth")
    this.#pickerController.refresh()
  }

  openProviderRecoveryPicker(provider: RottweilerState["providers"][number]): void {
    this.#providerRecoveryProvider = provider
    this.#pickerController.begin("providerRecovery")
    this.#pickerController.refresh()
  }

  openProviderApiKeyPrompt(provider: string): void {
    if (this.#state.replay.active || provider.length === 0) return
    this.#pickerController.begin("providerApiKey")
    this.#providerApiKeyProvider = provider
    this.picker.openSecret(`Enter ${providerName(provider)} API key`, (apiKey) => {
      const selectedProvider = this.#providerApiKeyProvider
      this.closePicker()
      if (selectedProvider !== null)
        void this.#submitProviderApiKey(selectedProvider, apiKey)
    })
  }

  openSettingsPicker(): void {
    this.#pickerController.begin("settings")
    this.#projectionRequests.command({ type: "list_settings" })
    this.#pickerController.refresh()
  }

  openPermissionPicker(): void {
    this.#pickerController.begin("permissions")
    this.#projectionRequests.command({ type: "list_permissions" })
    this.#pickerController.refresh()
  }

  openBudgetPicker(): void {
    this.#budgetSettingKey = null
    this.#pickerController.begin("budgets")
    this.#projectionRequests.command({ type: "list_settings" })
    this.#pickerController.refresh()
  }

  #openBudgetPresetPicker(key: BudgetSettingKey): void {
    this.#budgetSettingKey = key
    this.#pickerController.kind = "budgetPresets"
    this.#pickerController.refresh()
  }

  #openBudgetTextPrompt(key: BudgetSettingKey): void {
    this.#budgetSettingKey = key
    this.#pickerController.kind = "budgetInput"
    const prompt = key === "budget.session_cost_cap_micros_usd"
      ? "Session limit in USD, e.g. 12.50"
      : key === "budget.daily_cost_cap_micros_usd"
        ? "Daily limit in USD, e.g. 12.50"
        : "Warning threshold as a percent, e.g. 70"
    const placeholder = key === "budget.warn_at_percent" ? "70" : "12.50"
    this.picker.openTextPrompt(prompt, placeholder, (value) => {
      const selectedKey = this.#budgetSettingKey
      this.closePicker()
      if (selectedKey !== null) {
        this.#projectionRequests.command({ type: "set_setting", key: selectedKey, value })
      }
    }, 32)
  }

  openPermissionModePicker(): void {
    this.#pickerController.begin("permissionMode")
    this.#pickerController.refresh()
  }

  openTrustPicker(): void {
    this.#pickerController.begin("trust")
    this.#pickerController.refresh()
  }

  openTimelinePicker(): void {
    this.#timelineTurn = null
    this.#pickerController.begin("timeline")
    this.#pickerController.refresh()
  }

  openQueuedMessagesPicker(): void {
    if (this.#state.replay.active) return
    this.#pickerController.begin("queuedMessages")
    this.#pickerController.refresh()
  }

  openExportSessionPicker(): void {
    if (this.#state.replay.active) return
    this.#pickerController.begin("exportFormat")
    this.#pickerController.refresh()
  }

  #openExportPathPrompt(format: ExportFormat): void {
    const choice = EXPORT_FORMAT_CHOICES.find((item) => item.format === format)
    if (choice === undefined) return
    this.#pickerController.kind = "exportPath"
    this.picker.openTextPrompt(
      "Save to path, e.g. ~/transcript.md",
      `~/rottweiler-export.${choice.extension}`,
      (value) => {
        this.closePicker()
        const outputPath = expandLeadingHome(value.trim())
        void this.#submitSessionExport(format, outputPath, false)
      },
      4_096,
    )
  }

  async #submitSessionExport(
    format: ExportFormat,
    outputPath: string,
    force: boolean,
  ): Promise<void> {
    if (this.#state.replay.active) return
    const meta = this.#projectionRequests.meta()
    const pending: PendingExport = {
      format,
      outputPath,
      force,
      requestId: meta.request_id,
    }
    this.#pendingExport = pending
    try {
      const outcome = await this.#projectionRequests.emit({
        type: "export_session",
        meta,
        session_id: this.#sessionId,
        format,
        output_path: outputPath,
        force,
      })
      if (outcome?.type === "rejected" && this.#pendingExport?.requestId === meta.request_id) {
        this.#handleExportRejection(outcome, pending)
      } else if (outcome === null && this.#pendingExport?.requestId === meta.request_id) {
        this.#pendingExport = null
        this.#projectClientError(
          "session_export_unavailable",
          "the session export was not acknowledged by the engine",
          true,
        )
      }
    } catch (error) {
      if (this.#pendingExport?.requestId !== meta.request_id) return
      this.#pendingExport = null
      this.#projectClientError(
        "session_export_failed",
        `session export failed: ${safeErrorMessage(error)}`,
        true,
      )
    }
  }

  #handleExportRejection(outcome: Extract<CommandOutcome, { type: "rejected" }>, pending: PendingExport): void {
    this.#projectRejection(outcome)
    if (!pending.force && outcome.error.message.includes("export output already exists")) {
      this.#pendingExport = { ...pending, requestId: null }
      this.#pickerController.begin("exportOverwrite")
      this.#pickerController.refresh()
      return
    }
    this.#pendingExport = null
  }

  openWorkspaceRootsPicker(): void {
    this.#pickerController.begin("workspaceRoots")
    this.#pickerController.refresh()
  }

  openMcpPicker(): void {
    if (this.#state.replay.active) return
    this.#mcpActionName = null
    this.#clearMcpDraft()
    this.#pickerController.begin("mcp")
    this.#projectionRequests.command({ type: "list_mcp_servers" })
    this.#pickerController.refresh()
  }

  #openMcpActionPicker(server: string): void {
    if (this.#state.replay.active) return
    this.#mcpActionName = server
    this.#pickerController.kind = "mcpActions"
    this.#pickerController.refresh()
  }

  #openMcpHttpNamePrompt(): void {
    if (this.#state.replay.active) return
    this.#pickerController.kind = "mcpInput"
    this.picker.openTextPrompt(
      "Add remote MCP server",
      "server name",
      (name) => {
        if (!/^[A-Za-z0-9._-]{1,96}$/.test(name)) {
          this.#projectClientError(
            "mcp_name_invalid",
            "MCP server name is invalid"
          )
          return
        }
        this.#mcpDraftName = name
        this.picker.openTextPrompt(
          "Remote MCP endpoint",
          "https://example.com/mcp",
          (endpoint) => {
            const server = this.#mcpDraftName
            this.#mcpDraftName = null
            this.closePicker()
            if (server === null) return
            let parsed: URL
            try {
              parsed = new URL(endpoint)
            } catch {
              this.#projectClientError(
                "mcp_endpoint_invalid",
                "MCP endpoint must be an absolute HTTPS URL"
              )
              return
            }
            if (
              parsed.protocol !== "https:" ||
              parsed.username !== "" ||
              parsed.password !== "" ||
              parsed.search !== "" ||
              parsed.hash !== ""
            ) {
              this.#projectClientError(
                "mcp_endpoint_invalid",
                "MCP endpoint must be HTTPS without credentials, query, or fragment"
              )
              return
            }
            this.#projectionRequests.command({ type: "add_mcp_http_server", name: server, endpoint })
            this.openMcpPicker()
          }
        )
      }
    )
  }

  #openMcpStdioNamePrompt(): void {
    if (this.#state.replay.active) return
    this.#clearMcpDraft()
    this.#pickerController.kind = "mcpInput"
    this.picker.openTextPrompt(
      "Server name, e.g. docs",
      "docs",
      (name) => {
        if (!/^[A-Za-z0-9._-]{1,96}$/.test(name)) {
          this.#projectClientError("mcp_name_invalid", "MCP server name is invalid")
          return
        }
        this.#mcpDraftName = name
        this.picker.openTextPrompt(
          "Executable path, e.g. /usr/local/bin/docs-mcp",
          "/usr/local/bin/docs-mcp",
          (executable) => {
            this.#mcpDraftExecutable = executable
            this.picker.openTextPrompt(
              "Arguments separated by spaces · quoting is not supported · leave empty for none",
              "--stdio",
              (submitted) => {
                const argumentsText = submitted.startsWith(EMPTY_TEXT_PROMPT_SENTINEL)
                  ? submitted.slice(EMPTY_TEXT_PROMPT_SENTINEL.length)
                  : submitted
                const trimmed = argumentsText.trim()
                this.#mcpDraftArgs = trimmed.length === 0
                  ? []
                  : trimmed.split(/[\t\n\v\f\r ]+/)
                this.#openMcpEnvironmentPrompt()
              },
              64 * 1024,
            )
            this.picker.input.value = EMPTY_TEXT_PROMPT_SENTINEL
          },
          16 * 1024,
        )
      },
      96,
    )
  }

  #openMcpEnvironmentPrompt(): void {
    if (this.#state.replay.active) {
      this.closePicker()
      return
    }
    this.picker.openTextPrompt(
      "Environment variable as KEY=VALUE · leave empty to finish",
      "",
      (submitted) => {
        const entry = submitted.startsWith(EMPTY_TEXT_PROMPT_SENTINEL)
          ? submitted.slice(EMPTY_TEXT_PROMPT_SENTINEL.length)
          : submitted
        if (entry.length === 0) {
          const name = this.#mcpDraftName
          const executable = this.#mcpDraftExecutable
          const args = [...this.#mcpDraftArgs]
          const environment = [...this.#mcpDraftEnvironment]
          this.closePicker()
          if (name === null || executable === null) return
          this.#projectionRequests.command({
            type: "add_mcp_stdio_server",
            name,
            executable,
            args,
            environment,
          })
          this.openMcpPicker()
          return
        }
        const separator = entry.indexOf("=")
        if (separator <= 0) {
          this.#projectClientError(
            "mcp_environment_invalid",
            "Environment variable must use KEY=VALUE with a non-empty key",
          )
          this.#openMcpEnvironmentPrompt()
          return
        }
        this.#mcpDraftEnvironment.push({
          key: entry.slice(0, separator),
          value: entry.slice(separator + 1),
        })
        this.#openMcpEnvironmentPrompt()
      },
      16 * 1024 + 129,
    )
    this.picker.input.value = EMPTY_TEXT_PROMPT_SENTINEL
  }

  #clearMcpDraft(): void {
    this.#mcpDraftName = null
    this.#mcpDraftExecutable = null
    this.#mcpDraftArgs = []
    this.#mcpDraftEnvironment = []
  }

  #openPermissionPatternPrompt(
    action: PermissionAction
  ): void {
    this.#pickerController.kind = "permissionInput"
    this.picker.openTextPrompt(
      `Add ${action} permission rule`,
      "tool(glob), e.g. bash(cargo test*)",
      (pattern) => {
        this.closePicker()
        this.#projectionRequests.command({ type: "add_session_permission_rule", pattern, action })
      }
    )
  }

  openThemePicker(): void {
    this.#themeBeforePreview = this.#theme
    this.#themePreviewCommitted = false
    this.#pickerController.begin("themes")
    this.#rethemeInProgress = true
    this.#pickerController.refresh()
    const selectedIndex = this.picker.select.options.findIndex(
      (option) => option.value === `theme:${this.#theme.name}`,
    )
    if (selectedIndex >= 0) this.picker.select.setSelectedIndex(selectedIndex)
    this.#rethemeInProgress = false
  }

  #previewTheme(theme: RottweilerTheme): void {
    if (theme.name === this.#theme.name && this.#deferredTheme === null) return
    this.#createThemedSurface(theme)
  }

  async #confirmTheme(theme: RottweilerTheme): Promise<void> {
    const outcome = await this.#projectionRequests.emit({
      type: "set_setting",
      meta: this.#projectionRequests.meta(),
      session_id: this.#sessionId,
      key: "ui.theme",
      value: theme.name,
    })
    if (outcome?.type !== "accepted") {
      if (outcome?.type === "rejected") this.#projectRejection(outcome)
      else this.#projectClientError("theme_persistence_failed", "theme could not be persisted", true)
      this.closePicker()
      return
    }
    this.#themePreviewCommitted = true
    this.#themeBeforePreview = theme
    this.closePicker()
  }

  openModePicker(): void {
    this.#pickerController.begin("modes")
    this.#requestModes()
    this.#pickerController.refresh()
  }

  openSessionPicker(): void {
    this.#sessionActionId = null
    this.#pickerController.begin("sessions")
    this.#projectionRequests.command({ type: "list_sessions" })
    this.#pickerController.refresh()
  }

  #openSessionActionPicker(session: SessionProjection): void {
    this.#sessionActionId = session.sessionId
    this.#pickerController.kind = "sessionActions"
    this.#pickerController.refresh()
  }

  #openSessionRenamePrompt(session: SessionProjection): void {
    this.#sessionActionId = session.sessionId
    this.#pickerController.kind = "sessionRename"
    this.picker.openTextPrompt(
      "Rename session, e.g. Auth refactor",
      session.title ?? session.workspaceName,
      (title) => {
        const sessionId = this.#sessionActionId
        this.#pickerController.kind = "sessions"
        this.#pickerController.query = ""
        if (sessionId !== null) {
          this.#projectionRequests.command({ type: "rename_session", sessionId, title })
        }
        this.#pickerController.refresh()
      },
      288,
    )
  }

  openSubagentPicker(): void {
    if (this.#state.replay.active) {
      this.#projectClientError(
        "subagents_unavailable_in_replay",
        "Child-agent controls are available from the live parent session, not historical replay.",
      )
      return
    }
    this.#pickerController.begin("agents")
    this.#requestSubagents()
    this.#pickerController.refresh()
  }

  openSubagentActionPicker(subagentId = this.#activeSubagentId): void {
    if (subagentId === null || this.#subagentDescriptor(subagentId) === undefined) return
    this.#subagentActionId = subagentId
    this.#pickerController.begin("agentActions")
    this.#pickerController.refresh()
  }

  #openToolOutput(toolCallId: string): void {
    const tool = this.#presentedState().tools[toolCallId]
    if (tool === undefined) return
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
    this.setState(this.#state)
    this.#projectionRequests.command({ type: "get_session_review" })
  }

  closePicker(): void {
    this.#pickerController.close()
  }

  #afterPickerClosed(kind: PickerKind | null): void {
    const restoreTheme =
      (kind === "themes" || kind === "settings")
        && !this.#themePreviewCommitted
        ? this.#themeBeforePreview
        : null
    this.#projectionRequests.clear("files")
    this.#projectionRequests.setFilePreview(null)
    this.#providerApiKeyProvider = null
    this.#providerRecoveryProvider = null
    this.#clearMcpDraft()
    this.#mcpActionName = null
    this.#budgetSettingKey = null
    this.#subagentActionId = null
    this.#sessionActionId = null
    this.#themeBeforePreview = null
    this.#themePreviewCommitted = false
    if (
      restoreTheme !== null &&
      (restoreTheme.name !== this.#theme.name || this.#deferredTheme !== null)
    ) {
      this.#createThemedSurface(restoreTheme)
    }
    if (this.#keybindings.preset === "vim") this.#vimFocus = this.#vimFocusBeforePicker
    if (!this.#state.replay.active) this.#focusForInputMode()
    if (this.#keybindings.preset === "vim") {
      this.statusLine.setKeybindingMode(
        this.#inputMode === "normal" ? "normal" : "insert",
        this.#statusFocusOwner(),
      )
      this.statusLine.update(this.#presentedState())
    }
  }

  protected override onResize(width: number, height: number): void {
    this.contextPanel.visible =
      this.#activeSubagentId === null &&
      !this.#state.replay.active &&
      contextPanelHasContent(this.#state) &&
      width >= 100 &&
      height >= 12
    this.composer.resizeForTerminal(height)
    this.interactionPanel.resizeForTerminal(
      height,
      this.interactionPanel.usesComposer && this.composer.visible ? this.composer.dockHeight : 0,
    )
    this.outputViewer.resizeForTerminal(height)
    this.reviewPanel.resizeForTerminal(height)
    if (this.picker.visible) this.#pickerController.position(this.#pickerController.anchored)
  }

  override destroy(): void {
    if (this.#destroyed) return
    this.#destroyed = true
    this.#presentation.destroy()
    this.#clearPendingShellTimer()
    this.#clearPluginNotificationTimer()
    this.#clearSessionSearchTimer()
    this.#clearRuntimeServicesTimer()
    this.#clearInterruptEscape(false)
    this.#clearClipboardNotice()
    this.#clearExportNotice()
    this.ctx.off(CliRenderEvents.FOCUS, this.#onTerminalFocus)
    this.ctx.off(CliRenderEvents.BLUR, this.#onTerminalBlur)
    this.ctx.off(CliRenderEvents.THEME_MODE, this.#onTerminalThemeMode)
    this.ctx.off(CliRenderEvents.SELECTION, this.#onSelection)
    this.ctx.keyInput.off("keypress", this.#onGlobalKey)
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

  #showExportNotice(path: string): void {
    this.#clearExportNotice()
    this.banner.visible = true
    this.banner.fg = this.#theme.success
    this.banner.content = `Exported to ${path}`
    this.#exportNoticeTimer = setTimeout(() => {
      this.#exportNoticeTimer = null
      if (!this.#destroyed) this.setState(this.#state)
    }, 3_000)
  }

  #clearExportNotice(): void {
    if (this.#exportNoticeTimer === null) return
    clearTimeout(this.#exportNoticeTimer)
    this.#exportNoticeTimer = null
  }

  #keybindingContext(): KeybindingContext {
    if (this.#keybindings.preset === "standard") {
      return this.#outputViewerToolCallId !== null || this.#reviewOpen ? "review" : "standard"
    }
    if (this.picker.visible && !this.#pickerController.anchored) {
      return this.#inputMode === "insert" ? "picker_insert" : "picker_normal"
    }
    if (this.#outputViewerToolCallId !== null || this.#reviewOpen) return "review"
    return this.#inputMode === "insert" ? "vim_insert" : "vim_normal"
  }

  #handleKeybindingAction(action: KeybindingAction): boolean {
    if (action === "close_overlay") {
      if (this.#outputViewerToolCallId !== null) {
        this.#closeOutputViewer()
        return true
      }
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
    if (action === "open_subagent_picker") {
      this.openSubagentPicker()
      return true
    }
    if (action === "block_previous" || action === "block_next" || action === "block_toggle") {
      const focusOwner = this.#visibleFocusOwner()
      if (
        (this.#keybindings.preset === "vim" && focusOwner !== "transcript") ||
        (this.#keybindings.preset === "standard" && focusOwner !== "composer")
      ) return false
      if (action === "block_previous") this.transcript.selectPreviousBlock()
      else if (action === "block_next") this.transcript.selectNextBlock()
      else this.transcript.toggleSelectedBlock()
      return true
    }
    if (this.#state.replay.active) {
      return this.#handleReplayNavigation(action)
    }
    switch (action) {
      case "cycle_agent_mode": {
        const mode: ModeId = nextModeId(this.#state.mode, this.#state.modes)
        this.#projectionRequests.emit({
          type: "switch_mode",
          meta: this.#projectionRequests.meta(),
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
        // Let the terminal's normal text-paste path continue. When the
        // clipboard contains an image pasteImage attaches it asynchronously;
        // when it does not, Ctrl-V must remain ordinary text paste.
        return false
      case "open_external_editor":
        if (
          this.picker.visible ||
          this.#outputViewerToolCallId !== null ||
          this.#reviewOpen
        ) return false
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
        if (this.#keybindings.preset === "standard") this.transcript.scrollTo(0)
        else this.#moveToBoundary(false)
        return true
      case "view_bottom":
        if (this.#keybindings.preset === "standard") {
          this.transcript.scrollTo(this.transcript.scroller.scrollHeight)
        } else {
          this.#moveToBoundary(true)
        }
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
        this.transcript.scrollTo(0)
        return true
      case "view_bottom":
        this.transcript.scrollTo(this.transcript.scroller.scrollHeight)
        return true
      default:
        return false
    }
  }

  #setInputMode(mode: Exclude<InputMode, "standard">): void {
    if (this.#keybindings.preset !== "vim") return
    this.#inputMode = mode
    this.#focusForInputMode()
    this.statusLine.setKeybindingMode(mode, this.#statusFocusOwner())
    this.statusLine.update(this.#presentedState())
  }

  #focusForInputMode(): void {
    if (this.outputViewer.visible) {
      this.outputViewer.focusPresentation()
      return
    }
    if (this.reviewPanel.visible) {
      this.reviewPanel.focusPresentation()
      return
    }
    if (this.interactionPanel.capturesInput) {
      this.interactionPanel.select.focus()
      return
    }
    if (this.#isActiveSubagentRunning()) {
      this.composer.editor.showCursor = false
      this.transcript.scroller.focus()
      return
    }
    if (this.#inputMode === "standard") {
      this.composer.editor.showCursor = true
      this.composer.focus()
      return
    }
    if (this.picker.visible && !this.#pickerController.anchored) {
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
    this.statusLine.update(this.#presentedState())
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
    this.transcript.scrollBy(direction, unit)
  }

  #moveToBoundary(end: boolean): void {
    if (this.picker.visible) {
      this.picker.moveToBoundary(end)
    } else if (this.#vimFocus === "composer") {
      if (end) this.composer.editor.gotoBufferEnd()
      else this.composer.editor.gotoBufferHome()
    } else {
      this.transcript.scrollTo(end ? this.transcript.scroller.scrollHeight : 0)
    }
  }

  #restoreFocusAfterTranscriptInteraction(): void {
    if (this.#destroyed || this.#state.replay.active) return
    if (this.#inputMode === "standard") {
      this.#focusForInputMode()
      return
    }
    this.#vimFocus = "transcript"
    this.#vimFocusBeforePicker = "transcript"
    this.#focusForInputMode()
    this.statusLine.setKeybindingMode(this.#inputMode, "transcript")
    this.statusLine.update(this.#presentedState())
  }

  #visibleFocusOwner(): VimFocus | "interaction" | "output" | "review" {
    if (this.picker.visible && !this.#pickerController.anchored) return "picker"
    if (this.outputViewer.visible) return "output"
    if (this.reviewPanel.visible) return "review"
    if (this.interactionPanel.capturesInput) return "interaction"
    if (this.#state.replay.active) return "transcript"
    if (this.#isActiveSubagentRunning()) return "transcript"
    return this.#vimFocus
  }

  #statusFocusOwner(): VimFocus | "interaction" | "review" {
    const owner = this.#visibleFocusOwner()
    return owner === "output" ? "review" : owner
  }

  #renderPicker(): void {
    switch (this.#pickerController.kind) {
      case "palette":
        const paletteActions = this.#paletteActions()
        const paletteItems: PickerItem<PaletteAction | null>[] = []
        for (const section of PALETTE_SECTIONS) {
          const sectionActions = paletteActions.filter((action) => action.section === section)
          if (sectionActions.length === 0) continue
          paletteItems.push({
            id: `palette.section.${section.toLocaleLowerCase().replace(/[^a-z0-9]+/g, "-")}`,
            label: section,
            description: "",
            value: null,
            selectable: false,
            sectionHeader: true,
          })
          paletteItems.push(...sectionActions.map((action) => ({
            id: action.id,
            label: action.title,
            description: action.description,
            searchText: `${action.section} ${action.title} ${action.description}`,
            value: action,
          })))
        }
        this.#pickerController.show(
          "Command palette",
          paletteItems,
          (item) => item.value?.run(),
        )
        break
      case "keyboardHelp": {
        const items: PickerItem<null>[] = []
        for (const context of KEYBOARD_HELP_CONTEXTS[this.#keybindings.preset]) {
          const bindings = this.#keybindings.bindings(context)
          if (bindings.size === 0) continue
          items.push({
            id: `keyboard-help.section.${context}`,
            label: KEYBOARD_HELP_CONTEXT_NAMES[context],
            description: "",
            value: null,
            selectable: false,
            sectionHeader: true,
          })
          for (const [stroke, action] of bindings) {
            const keycap = formatKeycap(stroke)
            const label = KEYBINDING_ACTION_LABELS[action]
            items.push({
              id: `keyboard-help.${context}.${stroke}`,
              label: keycap,
              description: label,
              searchText: `${keycap} ${label}`,
              value: null,
            })
          }
        }
        this.#pickerController.show("Keyboard shortcuts", items, () => this.closePicker())
        break
      }
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
          ...mergeSlashCommandChoices(this.#state.commands).map((command) => ({
            id: command.name,
            label: `/${command.name}`,
            description: `${commandSourceLabel(command.source)} · ${command.description}`,
            searchText: command.usage,
            value: command,
          })),
        ]
        this.#pickerController.show(
          this.#state.commandsTruncated ? "Commands · results truncated" : "Commands",
          commandItems,
          (item) => {
            const command = item.value as CommandChoice | null
            if (command === null) {
              this.#requestCommands()
              return
            }
            const clearAnchoredTrigger = () => {
              if (this.#pickerController.anchored) this.composer.value = ""
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
            if (command.name === "rewind") {
              clearAnchoredTrigger()
              this.closePicker()
              this.openTimelinePicker()
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
            if (command.name === "agents") {
              clearAnchoredTrigger()
              this.closePicker()
              this.openSubagentPicker()
              return
            }
            if (command.name === "theme") {
              clearAnchoredTrigger()
              this.closePicker()
              this.openThemePicker()
              return
            }
            if (command.name === "settings") {
              clearAnchoredTrigger()
              this.closePicker()
              this.openSettingsPicker()
              return
            }
            if (command.name === "mode") {
              clearAnchoredTrigger()
              this.closePicker()
              this.openModePicker()
              return
            }
            const content = `/${command.name}`
            const requiresArgument = /<[^>]+>/.test(command.usage)
            if (this.#pickerController.anchored && !requiresArgument) {
              this.composer.value = content
              this.closePicker()
              void this.composer.submit()
              return
            }
            this.composer.value = `${content} `
            this.closePicker()
          },
        )
        break
      case "timeline": {
        const turns = this.#timelineTurns()
        if (turns.length === 0) {
          this.#pickerController.showStatus(
            "Conversation timeline",
            "No completed user turns",
            this.#state.replay.active ? "read-only session" : "Send a message to create a checkpoint.",
          )
          break
        }
        const readOnly = this.#state.replay.active
        const items: PickerItem<TimelineTurnChoice | null>[] = [
          ...(readOnly
            ? [{
                id: "timeline.read-only",
                label: "read-only session",
                description: "Timeline actions are unavailable in replay",
                value: null,
                selectable: false,
              }]
            : []),
          ...turns.map((turn) => ({
            id: `timeline.turn.${turn.sequenceId}`,
            label: timelineTurnLabel(turn.content),
            description: this.#timelineTurnDescription(turn.agentTurn, readOnly),
            value: turn,
            selectable: !readOnly,
          })),
        ]
        this.#pickerController.show("Conversation timeline", items, (item) => {
          if (item.value === null || readOnly) return
          this.#timelineTurn = item.value
          this.#pickerController.kind = "timelineActions"
          this.#pickerController.refresh()
        })
        break
      }
      case "timelineActions": {
        const turn = this.#timelineTurn
        if (turn === null) {
          this.openTimelinePicker()
          break
        }
        const items: PickerItem<TimelineAction>[] = [
          {
            id: "timeline.action.edit",
            label: "Edit and resend",
            description: "Rewind, restore the message in the composer, and focus it",
            value: "edit",
          },
          {
            id: "timeline.action.retry",
            label: "Retry",
            description: "Rewind and resend the same text without attachments",
            value: "retry",
          },
          {
            id: "timeline.action.rewind",
            label: "Rewind only",
            description: "Rewind without restoring the message",
            value: "rewind",
          },
        ]
        this.#pickerController.show(`Turn ${turn.agentTurn} actions`, items, (item) => {
          this.closePicker()
          void this.#startRewindIntent(turn, item.value)
        })
        break
      }
      case "queuedMessages": {
        if (this.#state.replay.active) {
          this.closePicker()
          break
        }
        const queuedMessages = this.#state.queuedMessages
        if (queuedMessages.length === 0) {
          this.#pickerController.showStatus(
            "Queued messages",
            "No queued messages",
            "Messages sent during an active turn will appear here.",
          )
          break
        }
        const items: PickerItem<QueuedMessagePickerAction>[] = [
          ...queuedMessages.map((message) => ({
            id: `queued.message.${message.position}`,
            label: queuedMessageLabel(message.content),
            description: "queued",
            value: { kind: "remove", position: message.position } as const,
          })),
          ...(queuedMessages.length < 2
            ? []
            : [{
                id: "queued.messages.clear",
                label: "Clear all queued messages",
                description: "Remove every queued message",
                value: { kind: "clear" } as const,
              }]),
        ]
        this.#pickerController.show("Queued messages · select to remove", items, (item) => {
          if (item.value.kind === "clear") {
            this.closePicker()
            this.#projectionRequests.command({ type: "clear_queued_messages" })
            return
          }
          this.#projectionRequests.command({
            type: "remove_queued_message",
            position: item.value.position,
          })
        })
        break
      }
      case "exportFormat": {
        if (this.#state.replay.active) {
          this.closePicker()
          break
        }
        this.#pickerController.show(
          "Export session",
          EXPORT_FORMAT_CHOICES.map((choice) => ({
            id: `export.format.${choice.format}`,
            label: choice.label,
            description: choice.description,
            value: choice.format,
          })),
          (item) => this.#openExportPathPrompt(item.value),
        )
        break
      }
      case "exportOverwrite": {
        const pending = this.#pendingExport
        if (this.#state.replay.active || pending === null) {
          this.closePicker()
          break
        }
        this.#pickerController.show(
          "Overwrite existing file?",
          [
            {
              id: "export.overwrite.confirm",
              label: "Overwrite",
              description: "Replace the existing file atomically",
              value: true,
            },
            {
              id: "export.overwrite.cancel",
              label: "Cancel",
              description: "Keep the existing file",
              value: false,
            },
          ],
          (item) => {
            this.closePicker()
            this.#pendingExport = null
            if (item.value) {
              void this.#submitSessionExport(pending.format, pending.outputPath, true)
            }
          },
        )
        break
      }
      case "exportPath":
        break
      case "workspaceRoots": {
        const workspaceRoots = this.#state.workspaceRoots
        if (workspaceRoots === null) {
          this.#pickerController.showLoading("Workspace roots", "Loading workspace roots")
          break
        }
        this.#pickerController.show(
          "Workspace roots",
          workspaceRoots.roots.map((root, index) => ({
            id: `workspace.root.${index}`,
            label: root,
            description: index === 0 ? "primary" : "additional",
            value: root,
          })),
          () => this.closePicker(),
        )
        break
      }
      case "files":
        const fileError = this.#projectionErrors.files
        if (
          fileError === undefined &&
          this.#projectionRequests.current("files") !== null &&
          this.#state.workspaceFiles.length === 0
        ) {
          this.#pickerController.showLoading("Workspace files", "Searching workspace files")
          break
        }
        if (fileError === undefined && this.#state.workspaceFiles.length === 0) {
          this.#pickerController.showStatus(
            "Workspace files",
            "No matching files",
            "Try a different search.",
          )
          break
        }
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
        this.#pickerController.show(
          "Workspace files",
          fileItems,
          (item) => {
            const file = item.value as RottweilerState["workspaceFiles"][number] | null
            if (file === null) {
              this.openFilePicker(this.#pickerController.query, this.#pickerController.anchored)
              return
            }
            if (file.isDirectory) {
              const query = `${file.path.replace(/\/$/, "")}/`
              if (this.#pickerController.anchored) {
                const mention = this.composer.currentFileMention()
                if (mention !== null) {
                  this.composer.replaceRange(mention.start, mention.end, `@${query}`)
                }
              } else {
                this.openFilePicker(query)
              }
              return
            }
            const draft = this.composer.value
            const mention = this.#pickerController.anchored ? this.composer.currentFileMention() : null
            const requestId = this.#projectionRequests.command({
              type: "preview_workspace_file",
              path: file.path,
              max_bytes: 5_242_880,
            })
            if (requestId !== null) {
              this.#projectionRequests.setFilePreview({
                path: file.path,
                requestId,
                draft,
                mention: mention === null ? null : { start: mention.start, end: mention.end },
              })
            }
          }
        )
        break
      case "attachments": {
        const attachments = this.composer.attachments
        const items: PickerItem<number>[] = attachments.map((attachment, index) => ({
          id: `attachment:${index}`,
          label: `Remove ${attachment.source_path ?? attachment.name}`,
          description: `${attachment.media_type} · remove only this attachment`,
          value: index,
        }))
        if (items.length === 0) {
          this.#pickerController.showStatus(
            "Attachments",
            "No attachments in this draft",
            "Paste an image or select a file with @ to attach it.",
          )
          break
        }
        this.#pickerController.show("Attachments", items, (item) => {
          this.composer.removeAttachment(item.value as number)
          if (this.composer.attachments.length === 0) this.closePicker()
          else this.#pickerController.refresh()
        })
        break
      }
      case "models":
        const models = this.#state.models.filter(
          (model) =>
            this.#modelProviderFilter === null ||
            (model.provider === undefined
              ? model.providers.includes(this.#modelProviderFilter)
              : model.provider === this.#modelProviderFilter),
        )
        const concreteModelIds = new Set(models.map((model) => model.id ?? model.alias))
        const aliases = this.#modelProviderFilter === null
          ? this.#state.modelAliases.filter(
              (alias) =>
                alias.candidates.length !== 1 ||
                alias.alias !== alias.candidates[0] ||
                !concreteModelIds.has(alias.candidates[0]!),
            )
          : []
        const modelItems: PickerItem<ModelPickerChoice | null>[] = [
          ...(aliases.length === 0
            ? []
            : [{
                id: "models.section.failover-chains",
                label: "Failover chains",
                description: "",
                value: null,
                selectable: false,
                sectionHeader: true,
              }]),
          ...aliases.map((alias) => ({
            id: `model-alias:${alias.alias}`,
            label: `${alias.current ? "● " : ""}${alias.alias}`,
            description: modelAliasDescription(alias, models),
            value: { kind: "alias" as const, alias },
          })),
          ...(models.length === 0
            ? []
            : [{
                id: "models.section.models",
                label: "Models",
                description: "",
                value: null,
                selectable: false,
                sectionHeader: true,
              }]),
          ...models.map((model) => ({
            id: model.id ?? model.alias,
            label: `${model.current === true ? "● " : ""}${model.displayName ?? model.alias}`,
            description: [
              model.provider ?? model.providers[0] ?? "unconfigured",
              modelAvailabilityLabel(model),
              model.toolCalling ? "tools" : "",
              model.vision ? "vision" : "",
              model.thinking ? "thinking" : "",
              "pinned route",
            ]
              .filter(Boolean)
              .join(" · "),
            value: { kind: "model" as const, model },
          })),
        ]
        const modelError = this.#projectionErrors.models
        if (modelError === undefined && this.#modelsRequested && modelItems.length === 0) {
          this.#pickerController.showLoading("Models", "Loading available models")
          break
        }
        if (modelError !== undefined) {
          modelItems.unshift({
            id: "models.error",
            label: "Couldn't load models",
            description: `${modelError} · select to retry`,
            value: null,
          })
        }
        if (modelItems.length === 0) {
          this.#pickerController.showStatus(
            "Models",
            "No models are available",
            "Connect a provider, then reopen this panel.",
          )
          break
        }
        this.#pickerController.show(
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
              this.#projectionRequests.command({
                type: "switch_model",
                model: selection.alias.alias,
              })
            } else {
              const model = selection.model
              if (model.available === false) {
                this.#projectClientError(
                  "model_unavailable",
                  model.status ?? `${model.displayName ?? model.alias} is unavailable`,
                  true,
                )
                return
              }
              this.#projectionRequests.command({
                type: "switch_model",
                model: model.id ?? model.alias,
                provider: model.provider ?? this.#modelProviderFilter,
              })
            }
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
              label: providerDisplayName(provider),
              description: [
                providerConnectionStatus(provider),
                `${provider.modelCount} model${provider.modelCount === 1 ? "" : "s"}`,
              this.#storedProviderKeys.has(
                provider.name)
                ? "credential stored"
                : "",
              providerStatusDetail(provider),
              ].filter(Boolean).join(" · "),
              value: provider,
            }))
        const providerError = this.#projectionErrors.models
        if (providerError === undefined && this.#modelsRequested && providerItems.length === 0) {
          this.#pickerController.showLoading("Providers", "Loading provider connections")
          break
        }
        if (providerError !== undefined) {
          providerItems.unshift({
            id: "providers.error",
            label: "Couldn't load providers",
            description: `${providerError} · select to retry`,
            value: null,
          })
        }
        if (providerItems.length === 0) {
          this.#pickerController.showStatus(
            "Providers",
            "No providers are connected",
            "Connect a provider, then reopen this panel.",
          )
          break
        }
        this.#pickerController.show(
          this.#providerPickerOnboarding
            ? "Welcome to Rottweiler · connect a provider to start"
            : "Providers",
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
            if (provider.authenticated && !provider.reachable) {
              this.openProviderRecoveryPicker(provider)
              return
            }
            switch (provider.nextAction) {
              case "select_models":
                this.openModelPicker(provider.name)
                break
              case "authenticate":
                this.#projectionRequests.command({ type: "begin_provider_auth", provider: provider.name })
                break
              case "api_key_cli":
                if (this.#storedProviderKeys.has(provider.name)) {
                  void this.#retryProviderActivation(provider.name)
                } else {
                  this.openProviderApiKeyPrompt(provider.name)
                }
                break
              case "configure":
                this.#projectionRequests.command({ type: "configure_builtin_provider", provider: provider.name })
                break
              case "none":
                this.#projectClientError(
                  "provider_auth_unavailable",
                  provider.status ?? `${provider.name} has no safe authentication action`,
                  true,
                )
                break
            }
          }
        )
        break
      }
      case "providerRecovery": {
        const provider = this.#providerRecoveryProvider
        if (provider === null) {
          this.openProviderPicker()
          break
        }
        const items: PickerItem<"activate" | "reauthenticate">[] = [
          {
            id: "provider-recovery.activate",
            label: "Refresh models",
            description: "Retry this provider's live model catalog with the saved sign-in",
            value: "activate",
          },
        ]
        if (provider.authKind !== "none") {
          items.push({
            id: "provider-recovery.reauthenticate",
            label: provider.authKind === "api_key" ? "Replace API key" : "Re-authenticate",
            description: "Replace the stored credential for this provider",
            value: "reauthenticate",
          })
        }
        this.#pickerController.show(`Reconnect ${providerName(provider.name)}`, items, (item) => {
          if (item.value === "activate") {
            void this.#retryProviderActivation(provider.name)
          } else if (provider.authKind === "api_key") {
            this.openProviderApiKeyPrompt(provider.name)
          } else {
            this.closePicker()
            this.#projectionRequests.command({ type: "begin_provider_auth", provider: provider.name })
          }
        })
        break
      }
      case "providerAuth": {
        const pending = this.#state.providerAuth.pending
        if (pending === null) {
          this.openProviderPicker()
          break
        }
        const authUrl =
          pending.challenge.kind === "oauth"
            ? pending.challenge.authorization_url
            : pending.challenge.verification_uri
        const prompt =
          pending.challenge.kind === "oauth"
            ? "Finish signing in in your browser; Rottweiler will continue automatically"
            : `Enter code ${pending.challenge.user_code} on GitHub; Rottweiler will continue automatically`
        const items: PickerItem<ProviderAuthPickerAction>[] = [
          {
            id: "provider-auth.open",
            label: pending.challenge.kind === "oauth" ? "Continue in browser" : "Open GitHub",
            description: this.#providerAuthActionNotice ?? prompt,
            searchText: `open browser ${prompt}`,
            value: { kind: "open_url", value: authUrl },
          },
        ]
        if (pending.challenge.kind === "device_flow") {
          items.push({
            id: "provider-auth.copy-code",
            label: `Copy code ${pending.challenge.user_code}`,
            description: "Copy the one-time GitHub device code",
            searchText: `copy code ${pending.challenge.user_code}`,
            value: { kind: "copy_code", value: pending.challenge.user_code },
          })
        }
        items.push(
          {
            id: "provider-auth.copy-url",
            label: "Copy sign-in link",
            description: "Copy the browser link to the clipboard",
            searchText: `copy url ${authUrl}`,
            value: { kind: "copy_url", value: authUrl },
          },
          {
            id: "provider-auth.cancel",
            label: "Cancel sign-in",
            description: pending.warnings.join(" · ") || "Stop this sign-in attempt",
            value: { kind: "cancel" },
          },
        )
        this.#pickerController.show(
          `Sign in · ${providerDisplayName({
            name: pending.provider,
            authKind: pending.challenge.kind === "oauth" ? "oauth" : "device_flow",
          })}`,
          items,
          (item) => {
            if (item.value.kind === "cancel") {
              this.#providerAuthActionNotice = null
              this.#projectionRequests.command({
                type: "cancel_provider_auth",
                provider: pending.provider,
                attemptId: pending.attemptId,
              })
            } else {
              void this.#runProviderAuthAction(
                pending.provider,
                pending.attemptId,
                item.value,
              )
            }
          },
        )
        break
      }
      case "providerApiKey":
        if (this.#providerApiKeyPending !== null) {
          this.#pickerController.showLoading(
            `Provider credential · ${providerName(this.#providerApiKeyPending)}`,
            "Storing and activating credential",
          )
        }
        break
      case "permissionInput":
        break
      case "mcpInput":
        break
      case "mcp": {
        const mcpError = this.#projectionErrors.mcp
        if (
          mcpError === undefined &&
          this.#projectionRequests.current("mcp") !== null &&
          this.#state.mcpServers.length === 0
        ) {
          this.#pickerController.showLoading("MCP connections", "Loading MCP connections")
          break
        }
        const items: PickerItem<McpPickerAction>[] = [
          ...(mcpError === undefined
            ? []
            : [{
                id: "mcp.error",
                label: "Couldn't load MCP connections",
                description: `${mcpError} · select to retry`,
                value: { kind: "retry" as const },
              }]),
          { id: "mcp.add.http", label: "Add HTTPS server…", description: "Remote HTTPS · registers live and starts disabled", value: { kind: "add_http" } },
          { id: "mcp.add.stdio", label: "Add stdio server…", description: "Local argv process · starts disabled with no extra sandbox authority", value: { kind: "add_stdio" } },
          ...(mcpError === undefined && this.#state.mcpServers.length === 0
            ? [{
                id: "mcp.empty",
                label: "No MCP servers configured",
                description: "Add an HTTPS or stdio server to connect tools and resources.",
                value: { kind: "add_stdio" as const },
                selectable: false,
              }]
            : []),
          ...this.#state.mcpServers.map<PickerItem<McpPickerAction>>((server) => ({
              id: `mcp.server.${server.name}`,
              label: server.name,
              description: `${server.approved ? "Approved" : "Approval needed"} · ${mcpStateLabel(server.state.type)} · ${server.tool_count} tools`,
              value: { kind: "actions", server: server.name },
          })),
        ]
        this.#pickerController.show(
          "MCP connections",
          items,
          (item) => {
            const action = item.value
            if (action.kind === "retry") this.#projectionRequests.command({ type: "list_mcp_servers" })
            else if (action.kind === "add_http") this.#openMcpHttpNamePrompt()
            else if (action.kind === "add_stdio") this.#openMcpStdioNamePrompt()
            else this.#openMcpActionPicker(action.server)
          },
        )
        break
      }
      case "mcpActions": {
        const server = this.#state.mcpServers.find(
          (candidate) => candidate.name === this.#mcpActionName,
        )
        if (server === undefined) {
          this.#mcpActionName = null
          this.#pickerController.kind = "mcp"
          this.#pickerController.refresh()
          break
        }
        const review = this.#state.mcpApprovalReview?.server === server.name
          ? this.#state.mcpApprovalReview
          : null
        const deferred = server.enabled && server.state.type === "disabled"
        const items: PickerItem<McpServerAction>[] = [
          {
            id: `mcp.toggle.${server.name}`,
            label: deferred || !server.enabled ? "Enable" : "Disable",
            description: `${mcpStateLabel(server.state.type)} · applies to this live session and persists after validation`,
            value: {
              kind: "toggle",
              server: server.name,
              enabled: deferred ? false : server.enabled,
            },
          },
          {
            id: `mcp.review.${server.name}`,
            label: "Review fingerprint",
            description: server.approved ? "Review the approved configuration identity" : "Review before approval",
            value: { kind: "review", server: server.name },
          },
          ...(review === null ? [] : [{
            id: `mcp.approve.${review.server}`,
            label: "Approve",
            description: `${mcpTransportLabel(review.transport)} · ${review.endpoint ?? "local process"} · configuration fingerprint ${review.fingerprint}`,
            value: { kind: "approve", server: review.server, fingerprint: review.fingerprint },
          }] satisfies PickerItem<McpServerAction>[]),
          {
            id: `mcp.remove.${server.name}`,
            label: "Remove",
            description: "Delete this server from the live session and user configuration",
            value: { kind: "remove", server: server.name },
          },
        ]
        this.#pickerController.show(
          `MCP actions · ${server.name}`,
          items,
          (item) => {
            const action = item.value
            if (action.kind === "toggle") {
              this.#projectionRequests.command({ type: "set_mcp_server_enabled", name: action.server, enabled: !action.enabled })
            } else if (action.kind === "review") {
              this.#projectionRequests.command({ type: "review_mcp_server", name: action.server })
            } else if (action.kind === "approve") {
              this.#projectionRequests.command({ type: "approve_mcp_server", name: action.server, fingerprint: action.fingerprint })
            } else {
              this.#mcpActionName = action.server
              this.#pickerController.kind = "mcpRemoveConfirm"
              this.#pickerController.refresh()
            }
          },
        )
        break
      }
      case "mcpRemoveConfirm": {
        const server = this.#mcpActionName
        if (server === null) {
          this.#pickerController.kind = "mcp"
          this.#pickerController.refresh()
          break
        }
        this.#pickerController.show(
          `Remove ${server}? This deletes its configuration`,
          [
            { id: "mcp.remove.confirm", label: "Remove", description: "Disable if needed, then delete", value: true },
            { id: "mcp.remove.cancel", label: "Cancel", description: "Keep this server", value: false },
          ],
          (item) => {
            if (item.value) {
              this.#projectionRequests.command({ type: "remove_mcp_server", name: server })
              this.#mcpActionName = null
              this.#pickerController.kind = "mcp"
            } else {
              this.#pickerController.kind = "mcpActions"
            }
            this.#pickerController.refresh()
          },
        )
        break
      }
      case "trust":
        this.#pickerController.show(
          "Folder trust",
          [
            {
              id: "trust.status",
              label: "Show trust status",
              description: "Display the current folder trust state",
              value: "/trust status",
            },
            {
              id: "trust.grant",
              label: "Grant trust",
              description: "Allow executable project configuration",
              value: "/trust grant",
            },
            {
              id: "trust.revoke",
              label: "Revoke trust",
              description: "Disable executable project configuration",
              value: "/trust revoke",
            },
          ],
          (item) => this.#submitPaletteCommand(item.value),
        )
        break
      case "permissionMode":
        this.#pickerController.show(
          "Permission mode",
          this.#permissionModeItems(),
          (item) => this.#selectPermissionMode(item.value.mode),
        )
        break
      case "permissionYoloConfirm":
        this.#pickerController.show(
          "Run every tool without asking?",
          [
            {
              id: "permissions.yolo.confirm",
              label: "Yes, enable yolo",
              description: "Never ask before tool use",
              value: true,
            },
            {
              id: "permissions.yolo.cancel",
              label: "Cancel",
              description: "Keep the current permission mode",
              value: false,
            },
          ],
          (item) => {
            if (item.value) this.#submitPaletteCommand("/permissions mode yolo")
            else this.closePicker()
          },
        )
        break
      case "permissions":
        {
          const permissions = this.#state.permissions
          const permissionError = this.#projectionErrors.permissions
          if (permissions === null && permissionError === undefined) {
            this.#pickerController.showLoading("Permission rules", "Loading permission rules")
            break
          }
          if (permissions === null) {
            this.#pickerController.showStatus(
              "Permission rules",
              "Permission rules could not be loaded",
              "Close and reopen this panel to retry.",
            )
            break
          }
          const items: PickerItem<PermissionPickerAction>[] = [
            ...this.#permissionModeItems(),
            {
              id: "permissions.refresh",
              label: `Default behavior · ${permissionActionLabel(permissions.default)}`,
              description: permissions.truncated === true
                ? "Inventory truncated · refresh after removing entries"
                : "Refresh effective rules and remembered approvals",
              value: { kind: "refresh" },
            },
            ...(["allow", "ask", "deny"] as const).map((action) => ({
              id: `permissions.add.${action}`,
              label: permissionRuleActionLabel(action),
              description: "Applies to this session · choose a tool or command pattern",
              value: { kind: "add", action } as const,
            })),
            ...(permissions?.effective_rules ?? []).map((rule) => ({
              id: `permissions.effective.${rule.id}`,
              label: `${permissionActionLabel(rule.action)} · ${permissionPatternLabel(rule.pattern)}`,
              description: "Trusted configuration · read-only",
              value: { kind: "info" } as const,
            })),
            ...(permissions?.project_rules ?? []).map((rule) => ({
              id: `permissions.project.${rule.id}`,
              label: `${permissionActionLabel(rule.action)} · ${permissionPatternLabel(rule.pattern)}`,
              description: "Project rule · read-only",
              value: { kind: "info" } as const,
            })),
            ...(permissions?.session_rules ?? []).map((rule) => ({
              id: `permissions.remove.${rule.id}`,
              label: `Remove · ${permissionPatternLabel(rule.pattern)}`,
              description: `This session · ${permissionActionLabel(rule.action).toLowerCase()} · select to remove`,
              value: { kind: "remove", ruleId: rule.id } as const,
            })),
            ...(permissions?.approvals ?? []).map((approval) => ({
              id: `permissions.revoke.${approval.id}`,
              label: `Revoke · ${approval.tool_name}`,
              description: `${approval.scope === "project" ? "This project" : "This session"} · remembered approval`,
              value: {
                kind: "revoke",
                approvalId: approval.id,
                scope: approval.scope,
              } as const,
            })),
          ]
        this.#pickerController.show(
          "Permission rules",
          items,
          (item) => {
            const action = item.value
            if (action.kind === "refresh") this.#projectionRequests.command({ type: "list_permissions" })
            else if (action.kind === "mode") {
              this.#selectPermissionMode(action.mode)
            }
            else if (action.kind === "add") this.#openPermissionPatternPrompt(action.action)
            else if (action.kind === "remove") {
              this.#projectionRequests.command({ type: "remove_session_permission_rule", ruleId: action.ruleId })
            } else if (action.kind === "revoke") {
              this.#projectionRequests.command({
                type: "revoke_permission_approval",
                approvalId: action.approvalId,
                scope: action.scope,
              })
            }
          }
        )
        }
        break
      case "budgets": {
        const rows = [
          {
            key: "budget.session_cost_cap_micros_usd",
            label: "Session limit",
            description: "Maximum spend for this session",
          },
          {
            key: "budget.daily_cost_cap_micros_usd",
            label: "Daily limit",
            description: "Maximum spend per UTC day",
          },
          {
            key: "budget.warn_at_percent",
            label: "Warn at",
            description: "Warn when a configured cap reaches this percentage",
          },
        ] as const
        const settings = rows.map((row) => ({
          ...row,
          setting: this.#state.settings.find((setting) => setting.key === row.key),
        }))
        if (settings.some(({ setting }) => setting === undefined)) {
          if (this.#projectionRequests.current("settings_pending") !== null) {
            this.#pickerController.showLoading("Budget limits", "Loading budget limits")
          } else {
            this.#pickerController.showStatus(
              "Budget limits",
              "Budget limits could not be loaded",
              "Close and reopen this panel to retry.",
            )
          }
          break
        }
        this.#pickerController.show(
          "Budget limits",
          settings.map(({ key, label, description, setting }) => ({
            id: `budget.setting.${key}`,
            label: `${label} · ${setting?.value}`,
            description: `${description} · ${setting?.provenance}${setting?.appliesImmediately ? " · live" : " · next session"}`,
            value: key,
          })),
          (item) => this.#openBudgetPresetPicker(item.value),
        )
        break
      }
      case "budgetPresets": {
        const key = this.#budgetSettingKey
        if (key === null) {
          this.openBudgetPicker()
          break
        }
        const isWarning = key === "budget.warn_at_percent"
        const title = key === "budget.session_cost_cap_micros_usd"
          ? "Session limit"
          : key === "budget.daily_cost_cap_micros_usd"
            ? "Daily limit"
            : "Warn at"
        const presets = isWarning
          ? [
              { label: "50%", value: "50" },
              { label: "75%", value: "75" },
              { label: "80%", value: "80" },
              { label: "90%", value: "90" },
              { label: "Custom…", value: null },
            ]
          : [
              { label: "$5", value: "5" },
              { label: "$10", value: "10" },
              { label: "$20", value: "20" },
              { label: "$50", value: "50" },
              { label: "$100", value: "100" },
              { label: "Unlimited", value: "unlimited" },
              { label: "Custom amount…", value: null },
            ]
        this.#pickerController.show(
          title,
          presets.map((preset) => ({
            id: `budget.preset.${key}.${preset.value ?? "custom"}`,
            label: preset.label,
            description: preset.value === null
              ? isWarning
                ? "Enter a custom warning percentage"
                : "Enter a USD amount with up to two decimals"
              : isWarning
                ? `Warn at ${preset.label} of either configured cap`
                : preset.value === "unlimited"
                  ? `Remove the ${title.toLowerCase()} cap`
                  : `Set the ${title.toLowerCase()} to ${preset.label}`,
            value: preset.value,
          })),
          (item) => {
            if (item.value === null) {
              this.#openBudgetTextPrompt(key)
              return
            }
            this.closePicker()
            this.#projectionRequests.command({ type: "set_setting", key, value: item.value })
          },
        )
        break
      }
      case "settings": {
        type SettingPickerAction =
          | { kind: "theme"; theme: RottweilerTheme }
          | { kind: "setting"; setting: RottweilerState["settings"][number]; value: string }
        const items: PickerItem<SettingPickerAction>[] = []
        for (const setting of this.#state.settings) {
          if (setting.key === "ui.theme") {
            for (const catalogTheme of themeCatalog) {
              const theme = this.#resolvedTheme(catalogTheme)
              items.push({
                id: `ui.theme:${theme.name}`,
                label: `Theme → ${theme.name}`,
                description: `${theme.name === this.#theme.name ? "current · " : ""}live preview · ${setting.provenance}`,
                value: { kind: "theme", theme },
              })
            }
            continue
          }
          for (const value of setting.choices) {
            items.push({
              id: `${setting.key}:${value}`,
              label: `${setting.label} → ${value}`,
              description: `${value === setting.value ? "current · " : ""}${setting.provenance}${setting.appliesImmediately ? " · live" : " · next session"}`,
              value: { kind: "setting", setting, value },
            })
          }
        }
        if (items.length === 0 && this.#projectionRequests.current("settings_pending") !== null) {
          this.#pickerController.showLoading("Settings", "Loading settings")
          break
        }
        if (items.length === 0) {
          this.#pickerController.showStatus(
            "Settings",
            "Settings could not be loaded",
            "Close and reopen this panel to retry.",
          )
          break
        }
        this.#pickerController.show("Settings", items, (item) => {
          const selection = item.value
          if (selection.kind === "theme") {
            if (this.#themeBeforePreview === null) this.#themeBeforePreview = this.#theme
            this.#previewTheme(selection.theme)
            void this.#confirmTheme(selection.theme)
            return
          }
          this.#projectionRequests.command({
            type: "set_setting",
            key: selection.setting.key,
            value: selection.value,
          })
        })
        break
      }
      case "themes": {
        const items: PickerItem<RottweilerTheme>[] = themeCatalog.map((catalogTheme) => {
          const theme = this.#resolvedTheme(catalogTheme)
          return {
            id: `theme:${theme.name}`,
            label: `${theme.name === this.#theme.name ? "● " : ""}${theme.name}`,
            description: `${theme.background} · ${theme.foreground} · ${theme.accent}`,
            value: theme,
          }
        })
        this.#pickerController.show("Themes · arrows preview · Enter confirms", items, (item) => {
          void this.#confirmTheme(item.value)
        })
        break
      }
      case "modes": {
        const presentation = modePickerPresentation(
          this.#state,
          this.#projectionErrors.modes,
          this.#projectionRequests.current("modes") !== null,
        )
        this.#pickerController.show(
          presentation.title,
          presentation.items,
          (item) => {
            if (item.value.kind === "retry") {
              this.#requestModes()
              this.#pickerController.refresh()
              return
            }
            this.#projectionRequests.emit({
              type: "switch_mode",
              meta: this.#projectionRequests.meta(),
              session_id: this.#sessionId,
              mode: item.value.id,
            })
            this.closePicker()
          },
        )
        break
      }
      case "agents": {
        if (this.#subagentListError !== null) {
          this.#pickerController.show(
            "Child agents · load failed",
            [{
              id: "agents.retry",
              label: "Retry loading child agents",
              description: boundedUiText(this.#subagentListError, 160),
              value: null,
            }],
            () => this.#requestSubagents(),
          )
          break
        }
        if (
          this.#projectionRequests.current("subagents") !== null &&
          this.#subagentDescriptors.length === 0
        ) {
          this.#pickerController.showLoading("Child agents", "Loading child agents")
          break
        }
        const items: PickerItem<SubagentDescriptor>[] = this.#subagentDescriptors.map((subagent) => ({
          id: subagent.subagent_id,
          label: subagent.task,
          description: `${subagent.activity === "running" ? "Running" : "Idle"} · ${subagent.agent} · ${subagent.model} · ${subagent.isolation}`,
          searchText: `${subagent.task} ${subagent.agent} ${subagent.model} ${subagent.activity}`,
          value: subagent,
        }))
        if (items.length === 0) {
          this.#pickerController.showStatus(
            "Child agents",
            "No child agents",
            "Child agents started by this session will appear here.",
          )
          break
        }
        this.#pickerController.show("Child agents · Enter to inspect", items, (item) => {
          this.closePicker()
          void this.#enterSubagent(item.value.subagent_id)
        })
        break
      }
      case "agentActions": {
        const subagent = this.#subagentActionId === null
          ? undefined
          : this.#subagentDescriptor(this.#subagentActionId)
        if (subagent === undefined) {
          this.closePicker()
          break
        }
        const items: PickerItem<SubagentAction>[] = [
          {
            id: "inspect",
            label: "Inspect transcript",
            description: "Open this child's live, typed event stream",
            value: { kind: "inspect", subagent },
          },
          ...(subagent.activity === "running"
            ? [{
                id: "running",
                label: "Child is still running",
                description: "Inspect progress or interrupt before sending a follow-up",
                value: { kind: "running", subagent } as SubagentAction,
                selectable: false,
              }]
            : [{
                id: "continue",
                label: "Resume with follow-up",
                description: "Focus the child composer; Enter sends to this child",
                value: { kind: "continue", subagent } as SubagentAction,
              }]),
          ...(subagent.activity === "running"
            ? [{
                id: "interrupt",
                label: "Interrupt child",
                description: "Stop the active child response",
                value: { kind: "interrupt", subagent } as SubagentAction,
              }]
            : []),
          {
            id: "close",
            label: "Close child",
            description: "Release this retained child agent",
            value: { kind: "close", subagent },
          },
        ]
        this.#pickerController.show(`Child actions · ${boundedUiText(subagent.task, 64)}`, items, (item) => {
          const action = item.value
          if (action.kind === "running") return
          this.closePicker()
          if (action.kind === "inspect") void this.#enterSubagent(action.subagent.subagent_id)
          else if (action.kind === "continue") {
            void this.#enterSubagent(action.subagent.subagent_id)
          } else if (action.kind === "interrupt") {
            void this.#interruptSubagent(action.subagent.subagent_id)
          } else {
            void this.#closeSubagent(action.subagent.subagent_id)
          }
        })
        break
      }
      case "sessions":
        const sessionError = this.#projectionErrors.sessions
        if (
          sessionError === undefined &&
          this.#projectionRequests.current("sessions") !== null &&
          this.#state.sessions.length === 0
        ) {
          this.#pickerController.showLoading("Sessions", "Loading sessions")
          break
        }
        if (sessionError === undefined && this.#state.sessions.length === 0) {
          this.#pickerController.showStatus(
            "Sessions",
            "No sessions found",
            "Start a conversation to create a session.",
          )
          break
        }
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
            label: session.title || session.workspaceName,
            description: `${session.workspaceName} · ${session.model}${session.shellActive ? " · shell active" : ""}`,
            searchText: `${session.sessionId} ${session.title ?? ""} ${session.workspaceName} ${session.model}`,
            value: session,
          })),
        ]
        this.#pickerController.show(
          this.#state.sessionSearch?.truncated === true
            ? "Sessions · results truncated"
            : "Sessions",
          sessionItems,
          (item) => {
            const session = item.value as RottweilerState["sessions"][number] | null
            if (session === null) {
              const query = this.picker.input.value.trim()
              if (query.length === 0) {
                this.#projectionRequests.command({ type: "list_sessions" })
              } else {
                this.#projectionRequests.command({ type: "search_sessions", query, limit: 100 })
              }
              return
            }
            this.#openSessionActionPicker(session)
          },
        )
        break
      case "sessionActions": {
        const session = this.#state.sessions.find(
          (candidate) => candidate.sessionId === this.#sessionActionId,
        )
        if (session === undefined) {
          this.closePicker()
          break
        }
        const items: PickerItem<SessionPickerAction>[] = [
          {
            id: "resume",
            label: "Resume session",
            description: "Switch to this session",
            value: { kind: "resume", session },
          },
          {
            id: "rename",
            label: "Rename session",
            description: "Change its picker title without switching",
            value: { kind: "rename", session },
          },
        ]
        this.#pickerController.show(
          `Session actions · ${boundedUiText(session.title ?? session.workspaceName, 64)}`,
          items,
          (item) => {
            if (item.value.kind === "resume") {
              this.closePicker()
              void this.#options.onSessionSelect?.(item.value.session.sessionId)
            } else {
              this.#openSessionRenamePrompt(item.value.session)
            }
          },
        )
        break
      }
      case "sessionRename":
        break
      case null:
        break
    }
  }

  #resolvedTheme(theme: RottweilerTheme): RottweilerTheme {
    if (theme.name === "system") return this.#systemTheme
    return themeByName(theme.name, this.#systemThemeMode ?? theme.mode) ?? theme
  }

  #timelineTurns(): readonly TimelineTurnChoice[] {
    return this.#state.transcript
      .filter(
        (entry) =>
          entry.turn.role === "user" &&
          this.#state.turns[entry.agentTurn]?.status !== "running" &&
          isU64(entry.agentTurn),
      )
      .map((entry) => {
        const message = timelineUserMessage(entry.turn)
        return {
          sequenceId: entry.sequenceId,
          agentTurn: entry.agentTurn,
          rewindTarget: (BigInt(entry.agentTurn) - 1n).toString(),
          content: message.content,
          hadAttachments: message.hadAttachments,
        }
      })
      .reverse()
  }

  #timelineTurnDescription(agentTurn: string, readOnly: boolean): string {
    const tools = Object.values(this.#state.tools).filter((tool) => tool.turnId === agentTurn)
    const edits = tools.filter((tool) => tool.diff !== null).length
    const detail = [`turn ${agentTurn}`]
    if (tools.length > 0) detail.push(`${tools.length} ${tools.length === 1 ? "tool" : "tools"}`)
    if (edits > 0) detail.push(`${edits} ${edits === 1 ? "edit" : "edits"}`)
    if (readOnly) detail.push("read-only")
    return detail.join(" · ")
  }

  async #startRewindIntent(turn: TimelineTurnChoice, action: TimelineAction): Promise<void> {
    const target = action === "rewind" ? turn.agentTurn : turn.rewindTarget
    const intent: PendingRewindIntent = {
      action,
      target,
      content: turn.content,
      hadAttachments: turn.hadAttachments,
      requestId: null,
    }
    this.#pendingRewindIntent = intent
    try {
      const accepted = await this.#sendMessage(`/rewind ${target}`, [], true)
      if (!accepted && this.#pendingRewindIntent === intent) this.#pendingRewindIntent = null
    } catch (error) {
      if (this.#pendingRewindIntent === intent) this.#pendingRewindIntent = null
      this.#projectClientError(
        "rewind_failed",
        presentError({
          category: "protocol",
          code: "rewind_failed",
          message: safeErrorMessage(error),
        }).text,
        true,
      )
    }
  }

  #permissionModeItems(): PickerItem<PermissionModePickerAction>[] {
    const current = this.#state.permissions?.runtime_mode ?? "default"
    return PERMISSION_MODE_CHOICES.map((choice) => ({
      id: `permissions.mode.${choice.mode}`,
      label: choice.mode === current ? `● ${choice.mode}` : choice.mode,
      description: choice.description,
      value: { kind: "mode", mode: choice.mode },
    }))
  }

  #selectPermissionMode(mode: PermissionMode): void {
    if (mode === "yolo") {
      this.#pickerController.anchored = false
      this.#pickerController.query = ""
      this.#pickerController.kind = "permissionYoloConfirm"
      this.#pickerController.refresh()
      return
    }
    this.#submitPaletteCommand(`/permissions mode ${mode}`)
  }

  #submitPaletteCommand(content: string): void {
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

  #paletteBinding(action: KeybindingAction): string | null {
    return this.#bindingHint(action, ["global"])
  }

  #bindingHint(action: KeybindingAction, contexts: readonly KeybindingContext[]): string | null {
    for (const context of contexts) {
      for (const [stroke, boundAction] of this.#keybindings.bindings(context)) {
        if (boundAction === action) return formatKeycap(stroke)
      }
    }
    return null
  }

  #composerKeybindingContext(): Extract<KeybindingContext, "standard" | "vim_insert"> {
    return this.#keybindings.preset === "vim" ? "vim_insert" : "standard"
  }

  #paletteDescription(description: string, binding?: KeybindingAction): string {
    if (binding === undefined) return description
    const hint = this.#paletteBinding(binding)
    return hint === null ? description : `${description} · ${hint}`
  }

  #paletteActions(): readonly PaletteAction[] {
    const open = (action: () => void) => () => {
      this.closePicker()
      action()
    }
    const submit = (content: string) => () => this.#submitPaletteCommand(content)
    const prefill = (content: string) => () => {
      this.closePicker()
      this.composer.value = `${content} `
      this.composer.focus()
    }
    const actions: PaletteAction[] = [
      ...(Object.values(this.#presentedState().turns).some((turn) => turn.status === "running")
        ? [{ id: "interrupt.run", title: "Interrupt turn", section: "Conversation", description: "Stop the active turn", run: submit("/interrupt") } satisfies PaletteAction]
        : []),
      { id: "compact.run", title: "Compact context", section: "Conversation", description: "Compact the conversation context", run: submit("/compact") },
      { id: "rewind.run", title: "Rewind to a turn", section: "Conversation", description: "Choose from completed user turns", run: open(() => this.openTimelinePicker()) },
      { id: "fork.run", title: "Fork session", section: "Conversation", description: "Fork at the latest completed turn", run: open(() => void this.#requestFork(null)) },
      { id: "session.list", title: "Switch session", section: "Conversation", description: this.#paletteDescription("Resume another durable session", "open_session_picker"), run: open(() => this.openSessionPicker()) },
      { id: "review.open", title: "Review changes", section: "Conversation", description: this.#paletteDescription("Open the cumulative session diff", "open_review"), run: open(() => this.openReview()) },
      { id: "session.export", title: "Export session", section: "Conversation", description: "Save this session's transcript to a file", run: open(() => this.openExportSessionPicker()) },
      { id: "plan.show", title: "Show plan", section: "Conversation", description: "Display the pending or approved plan", run: submit("/plan") },
      { id: "queue.manage", title: "Manage queued messages", section: "Conversation", description: "Review, remove, or clear queued messages", run: open(() => this.openQueuedMessagesPicker()) },
      { id: "cost.show", title: "Show usage & cost", section: "Conversation", description: "Display tokens, cost, and budget", run: submit("/cost") },

      { id: "model.list", title: "Switch model", section: "Agents & models", description: this.#paletteDescription("Choose the active model alias", "open_model_picker"), run: open(() => this.openModelPicker()) },
      { id: "provider.list", title: "Switch provider route", section: "Agents & models", description: "Choose a configured provider and model route", run: open(() => this.openProviderPicker()) },
      { id: "mode.list", title: "Switch mode", section: "Agents & models", description: this.#paletteDescription("Choose discuss, plan, or execute", "open_mode_picker"), run: open(() => this.openModePicker()) },
      { id: "agent.children", title: "Child agents", section: "Agents & models", description: this.#paletteDescription("Inspect, resume, interrupt, or close child agents", "open_subagent_picker"), run: open(() => this.openSubagentPicker()) },
      ...(this.#activeSubagentId === null ? [] : [{
        id: "agent.current.actions",
        title: "Current child actions",
        section: "Agents & models",
        description: "Inspect, continue, interrupt, or close the visible child",
        run: open(() => this.openSubagentActionPicker(this.#activeSubagentId)),
      } satisfies PaletteAction]),
      { id: "status.show", title: "Show agent status", section: "Agents & models", description: "Display running and queue state", run: submit("/status") },

      { id: "workspace.add", title: "Add workspace directory", section: "Workspace", description: "Prefills /add-dir · give a directory path", run: prefill("/add-dir") },
      { id: "workspace.roots", title: "Workspace roots", section: "Workspace", description: "See every live workspace root", run: open(() => this.openWorkspaceRootsPicker()) },
      { id: "trust.manage", title: "Folder trust", section: "Workspace", description: "Show, grant, or revoke folder trust", run: open(() => this.openTrustPicker()) },
      { id: "context.manage", title: "Manage context", section: "Workspace", description: "Inspect, pin, or evict context items", run: submit("/context") },

      { id: "permissions.mode", title: "Permission mode", section: "Safety", description: "Choose when tool use needs confirmation", run: open(() => this.openPermissionModePicker()) },
      { id: "permissions.manage", title: "Permission rules", section: "Safety", description: "Inspect, add, and remove session rules", run: open(() => this.openPermissionPicker()) },
      { id: "budget.manage", title: "Budget limits", section: "Safety", description: "Set session and daily spend caps", run: open(() => this.openBudgetPicker()) },

      { id: "theme.list", title: "Switch theme", section: "Appearance & settings", description: "Preview and choose an interface theme", run: open(() => this.openThemePicker()) },
      { id: "settings.open", title: "Settings", section: "Appearance & settings", description: "Change safe persisted user settings", run: open(() => this.openSettingsPicker()) },
      { id: "mcp.manage", title: "MCP connections", section: "Appearance & settings", description: "Add, review, enable, disable, or remove MCP servers", run: open(() => this.openMcpPicker()) },

      { id: "keyboard.help", title: "Keyboard shortcuts", section: "Help & system", description: "Every binding for the active preset", run: open(() => this.openKeyboardHelpPicker()) },
      { id: "help.show", title: "Command help", section: "Help & system", description: "List every available slash command", run: submit("/help") },
      { id: "app.exit", title: "Exit Rottweiler", section: "Help & system", description: "Close the TUI and its supervised engine", run: open(() => this.#options.onExit?.()) },
    ]
    for (const command of this.#state.commands) {
      if (isLocalSlashCommand(command.name)) continue
      const requiresArgument = /<[^>]+>/.test(command.usage)
      actions.push({
        id: `slash.${command.name}`,
        title: `/${command.name}`,
        section: "Commands",
        description: `${commandSourceLabel(command.source)} · ${command.description}`,
        run: requiresArgument ? prefill(`/${command.name}`) : submit(`/${command.name}`),
      })
    }
    return actions
  }

  #composerInputChanged(value: string): void {
    const changed = value !== this.#lastComposerValue
    this.#lastComposerValue = value
    if (!changed) {
      this.#updateComposerAutocomplete(value)
      return
    }
    this.#options.onComposerInput?.(value)
    this.transcript.clearBlockSelection()
    const hadPendingIntent = this.#pendingRewindIntent !== null
    this.#pendingRewindIntent = null
    const hadNotice = this.#composerNotice !== null
    this.#composerNotice = null
    if ((hadPendingIntent || hadNotice) && !this.#destroyed) this.setState(this.#state)
    this.#updateComposerAutocomplete(value)
  }

  #clearComposerNotice(): void {
    if (this.#composerNotice === null) return
    this.#composerNotice = null
    if (!this.#destroyed) this.setState(this.#state)
  }

  #updateComposerAutocomplete(value: string): void {
    const slash = /^\/([^\s]*)$/.exec(value)
    if (slash !== null) {
      this.#pickerController.anchored = true
      this.#pickerController.query = slash[1] ?? ""
      this.#pickerController.position(true)
      this.#pickerController.kind = "commands"
      if (this.#state.commands.length === 0 && !this.#commandsRequested) {
        this.#requestCommands()
      }
      this.#pickerController.refresh()
      return
    }
    const mention = /(?:^|\s)@([^\n]*)$/.exec(value)
    if (mention === null && this.#pickerController.anchored) this.closePicker()
  }

  #requestCommands(): void {
    this.#commandsRequested = true
    this.#clearProjectionError("commands")
    this.#projectionRequests.command({ type: "list_commands" })
  }

  #requestSubagents(): void {
    if (this.#state.replay.active) return
    this.#subagentListError = null
    const meta = this.#projectionRequests.issue("subagents")
    void this.#projectionRequests.emit({
      type: "list_subagents",
      meta,
      session_id: this.#sessionId,
    }).then((outcome) => {
      if (
        outcome?.type === "rejected" &&
        this.#projectionRequests.matches("subagents", meta.request_id)
      ) {
        this.#projectionRequests.clear("subagents")
        this.#subagentListError = presentError({
          category: outcome.error.category,
          code: outcome.error.code,
          message: outcome.error.message,
          requestId: meta.request_id,
        }).text
        this.#projectRejection(outcome)
        if (this.#pickerController.kind === "agents") this.#pickerController.refresh()
      } else if (
        outcome == null &&
        this.#projectionRequests.matches("subagents", meta.request_id)
      ) {
        this.#projectionRequests.clear("subagents")
        const presentation = presentError({
          category: "protocol",
          code: "subagents_unavailable",
          message: "Couldn't load child agents because the engine connection is unavailable.",
          requestId: meta.request_id,
        })
        this.#subagentListError = presentation.text
        this.#projectClientError(
          "subagents_unavailable",
          presentation.text,
          true,
        )
        if (this.#pickerController.kind === "agents") this.#pickerController.refresh()
      }
    }).catch((error) => {
      if (!this.#projectionRequests.matches("subagents", meta.request_id)) return
      this.#projectionRequests.clear("subagents")
      const presentation = presentError({
        category: "protocol",
        code: "subagents_failed",
        message: safeErrorMessage(error),
        requestId: meta.request_id,
      })
      this.#subagentListError = presentation.text
      this.#projectClientError("subagents_failed", presentation.text, true)
      if (this.#pickerController.kind === "agents") this.#pickerController.refresh()
    })
  }

  async #enterSubagent(subagentId: string): Promise<void> {
    const descriptor = this.#subagentDescriptor(subagentId)
    if (descriptor === undefined) return
    this.#saveComposerDraft()
    this.#activeSubagentId = subagentId
    this.#restoreComposerDraft(subagentId)
    this.#subagentErrorBaseline = this.#state.errors.at(-1)
    if (!this.#subagentStates.has(subagentId)) {
      this.#subagentStates.set(subagentId, initialSubagentState(this.#state, descriptor))
    }
    this.setState(this.#state)
    const effects = this.#transitionSubagentReplay(subagentId, {
      type: "enter",
      childSessionId: descriptor.child_session_id,
    })
    if (effects.some((effect) => effect.type === "resetProjection")) this.setState(this.#state)
    this.#focusForInputMode()
  }

  async #requestSubagentReplayPage(
    subagentId: string,
    afterSequence: string | null,
  ): Promise<void> {
    const meta = this.#projectionRequests.meta()
    this.#transitionSubagentReplay(subagentId, {
      type: "requestIssued",
      requestId: meta.request_id,
      afterSequence,
    })
    try {
      const outcome = await this.#projectionRequests.emit({
        type: "replay_subagent",
        meta,
        session_id: this.#sessionId,
        subagent_id: subagentId,
        after_sequence: afterSequence,
      })
      if (outcome?.type === "rejected") {
        const effects = this.#transitionSubagentReplay(subagentId, {
          type: "rejected",
          requestId: meta.request_id,
          failure: "rejected",
        })
        if (effects.some((effect) => effect.type === "replayFailed")) this.#projectRejection(outcome)
      } else if (outcome == null) {
        const effects = this.#transitionSubagentReplay(subagentId, {
          type: "rejected",
          requestId: meta.request_id,
          failure: "unavailable",
        })
        if (effects.some((effect) => effect.type === "replayFailed")) {
          const presentation = presentError({
            category: "protocol",
            code: "subagent_replay_unavailable",
            message: "Couldn't load the child transcript because the engine connection is unavailable.",
            requestId: meta.request_id,
          })
          this.#projectClientError(
            "subagent_replay_unavailable",
            presentation.text,
            true,
          )
        }
      }
    } catch (error) {
      const effects = this.#transitionSubagentReplay(subagentId, {
        type: "rejected",
        requestId: meta.request_id,
        failure: "exception",
      })
      if (effects.some((effect) => effect.type === "replayFailed")) {
        this.#projectClientError(
          "subagent_replay_failed",
          presentError({
            category: "protocol",
            code: "subagent_replay_failed",
            message: safeErrorMessage(error),
            requestId: meta.request_id,
          }).text,
          true,
        )
      }
    }
  }

  #markSubagentReplayTransportLost(): void {
    for (const subagentId of this.#subagentReplays.keys()) {
      this.#transitionSubagentReplay(subagentId, { type: "transportLost" })
    }
  }

  #recoverSubagentReplays(): void {
    for (const subagentId of [...this.#subagentReplays.keys()]) {
      if (this.#subagentDescriptor(subagentId) === undefined) {
        this.#subagentReplays.delete(subagentId)
        continue
      }
      this.#transitionSubagentReplay(subagentId, { type: "reconnected" })
    }
  }

  #leaveSubagent(): void {
    if (this.#activeSubagentId === null) return
    this.#saveComposerDraft()
    this.#activeSubagentId = null
    this.#restoreComposerDraft(null)
    this.#subagentActionId = null
    this.#subagentErrorBaseline = undefined
    this.setState(this.#state)
    this.#focusForInputMode()
  }

  #saveComposerDraft(): void {
    const draft: ComposerDraft = {
      content: this.composer.value,
      attachments: [...this.composer.attachments],
    }
    if (this.#activeSubagentId === null) this.#parentComposerDraft = draft
    else this.#subagentComposerDrafts.set(this.#activeSubagentId, draft)
  }

  #composerScope(): string {
    return this.#activeSubagentId === null ? "parent" : `child:${this.#activeSubagentId}`
  }

  #restoreDetachedSubmission(
    scope: string,
    content: string,
    attachments: readonly Attachment[],
  ): void {
    const childId = scope.startsWith("child:") ? scope.slice("child:".length) : null
    if (scope !== "parent" && (childId === null || this.#subagentDescriptor(childId) === undefined)) return
    const current = childId === null
      ? this.#parentComposerDraft
      : this.#subagentComposerDrafts.get(childId) ?? { content: "", attachments: [] }
    const restored = mergeComposerDraft(current, content, attachments)
    if (childId === null) this.#parentComposerDraft = restored
    else this.#subagentComposerDrafts.set(childId, restored)
  }

  #restoreComposerDraft(subagentId: string | null): void {
    const draft = subagentId === null
      ? this.#parentComposerDraft
      : this.#subagentComposerDrafts.get(subagentId) ?? { content: "", attachments: [] }
    this.composer.restoreDraft(draft.content, draft.attachments)
  }

  #applySubagentEvent(subagentId: string, event: WireEngineEvent): void {
    const descriptor = this.#subagentDescriptor(subagentId)
    if (descriptor === undefined) return
    const previous = this.#subagentStates.get(subagentId) ?? initialSubagentState(this.#state, descriptor)
    const next = boundSubagentState(reduceRottweilerState(previous, engineEvent(event)))
    this.#subagentStates.set(subagentId, next)
    this.#subagentErrorBaseline = this.#state.errors.at(-1)
    if (event.type === "turn_finished") this.#setSubagentActivity(subagentId, "idle")
    else if (event.type === "turn_started") this.#setSubagentActivity(subagentId, "running")
    this.#presentation.markDirty(deferPresentationForEvent(event))
  }

  #transitionSubagentReplay(
    subagentId: string,
    input: SubagentReplayInput<WireEngineEvent>,
  ): readonly SubagentReplayEffect<WireEngineEvent>[] {
    const descriptor = this.#subagentDescriptor(subagentId)
    const current = this.#subagentReplays.get(subagentId) ?? createSubagentReplayState(
      descriptor?.child_session_id ?? "",
      this.#lastAppliedSubagentSequence(subagentId),
    )
    const transition = transitionSubagentReplay(current, input)
    this.#subagentReplays.set(subagentId, transition.state)
    for (const effect of transition.effects) {
      switch (effect.type) {
        case "requestPage":
          void this.#requestSubagentReplayPage(subagentId, effect.afterSequence)
          break
        case "applyEvents":
        case "drainBuffer":
          for (const event of effect.events) this.#applySubagentEvent(subagentId, event)
          break
        case "resetProjection":
          if (descriptor !== undefined) {
            this.#subagentStates.set(subagentId, initialSubagentState(this.#state, descriptor))
          }
          break
        case "noticeRestart":
          this.#projectClientError("subagent_replay_gap", effect.reason, true)
          break
        case "bufferProgress":
        case "replayFailed":
        case "none":
          break
      }
    }
    return transition.effects
  }

  #lastAppliedSubagentSequence(subagentId: string): string | null {
    return this.#subagentStates.get(subagentId)?.lastSequence ?? null
  }

  #subagentDescriptor(subagentId: string): SubagentDescriptor | undefined {
    return this.#subagentDescriptors.find((subagent) => subagent.subagent_id === subagentId)
  }

  #isActiveSubagentRunning(): boolean {
    return this.#activeSubagentId !== null &&
      this.#subagentDescriptor(this.#activeSubagentId)?.activity === "running"
  }

  #setSubagentActivity(subagentId: string, activity: SubagentDescriptor["activity"]): void {
    this.#subagentDescriptors = this.#subagentDescriptors.map((subagent) =>
      subagent.subagent_id === subagentId ? { ...subagent, activity } : subagent,
    )
  }

  #presentedState(): RottweilerState {
    if (this.#activeSubagentId === null) return this.#state
    const descriptor = this.#subagentDescriptor(this.#activeSubagentId)
    if (descriptor === undefined) return this.#state
    return this.#subagentStates.get(this.#activeSubagentId) ?? initialSubagentState(this.#state, descriptor)
  }

  #updateSubagentBanner(state: RottweilerState): void {
    if (this.#activeSubagentId === null) return
    const descriptor = this.#subagentDescriptor(this.#activeSubagentId)
    if (descriptor === undefined) return
    const approval = Object.values(state.tools).some((tool) => tool.status === "awaiting_approval")
    const replay = this.#subagentReplays.get(this.#activeSubagentId)
    const replaying = replay?.status === "replaying"
    const retainedHistory = replay?.historyTruncatedAt
    const latestError = this.#state.errors.at(-1)
    const hasErrorContext = latestError !== undefined && latestError !== this.#subagentErrorBaseline
    const projection = this.#state.subagents[this.#activeSubagentId] ?? Object.values(
      this.#state.subagents,
    ).findLast((subagent) => subagent.subagentId === this.#activeSubagentId)
    const status = projection?.status.replaceAll("_", " ") ?? descriptor.activity
    const elapsed = projection?.status === "running"
      ? formatSubagentElapsed(projection.spawnedAtMs)
      : null
    const activity = replaying
      ? "loading transcript"
      : approval
        ? "approval requested by child"
        : projection?.activity ?? descriptor.activity
    const activitySegment = activity.trim()
    const detail = [
      status,
      ...(activitySegment === "" || activitySegment.toLowerCase() === status.trim().toLowerCase()
        ? []
        : [activitySegment]),
      ...(elapsed === null ? [] : [elapsed]),
      ...(status.toLowerCase() === "running" && !replaying && !approval && !hasErrorContext
        ? ["read-only", "interrupt to reply"]
        : []),
    ].join(" · ")
    const errorPresentation = hasErrorContext && latestError !== undefined
      ? presentError(latestError)
      : null
    const context = errorPresentation !== null
      ? errorPresentation.text
      : retainedHistory !== undefined && !replaying
        ? `recent activity · ${retainedHistory} earlier events retained`
        : null
    this.banner.visible = true
    this.banner.fg = errorPresentation !== null
      ? this.#theme[errorPresentation.severity]
      : approval
        ? this.#theme.warning
        : this.#theme.info
    const childrenHint = this.#paletteBinding("open_subagent_picker")
    const paletteHint = this.#paletteBinding("open_command_picker")
    const hints = [
      "Esc parent",
      ...(childrenHint === null ? [] : [`${childrenHint} children`]),
      ...(paletteHint === null ? [] : [`${paletteHint} palette`]),
    ]
    this.banner.content = t`${fg(this.#theme.accentStrong)("◉ child agent")} · ${descriptor.task} · ${detail}${context === null ? "" : ` · ${context}`} · ${hints.join(" · ")}`
  }

  async #interruptSubagent(subagentId: string): Promise<void> {
    let outcome: void | CommandOutcome | null
    try {
      outcome = await this.#projectionRequests.emit({
        type: "interrupt_subagent",
        meta: this.#projectionRequests.meta(),
        session_id: this.#sessionId,
        subagent_id: subagentId,
      })
    } catch (error) {
      this.#projectClientError(
        "subagent_interrupt_failed",
        presentError({
          category: "protocol",
          code: "subagent_interrupt_failed",
          message: safeErrorMessage(error),
        }).text,
        true,
      )
      return
    }
    if (outcome?.type === "rejected") this.#projectRejection(outcome)
    else if (outcome == null) {
      const presentation = presentError({
        category: "protocol",
        code: "subagent_interrupt_unavailable",
        message: "Couldn't interrupt the child because the engine connection is unavailable.",
      })
      this.#projectClientError(
        "subagent_interrupt_unavailable",
        presentation.text,
        true,
      )
    }
  }

  async #closeSubagent(subagentId: string): Promise<void> {
    let outcome: void | CommandOutcome | null
    try {
      outcome = await this.#projectionRequests.emit({
        type: "close_subagent",
        meta: this.#projectionRequests.meta(),
        session_id: this.#sessionId,
        subagent_id: subagentId,
      })
    } catch (error) {
      this.#projectClientError(
        "subagent_close_failed",
        presentError({
          category: "protocol",
          code: "subagent_close_failed",
          message: safeErrorMessage(error),
        }).text,
        true,
      )
      return
    }
    if (outcome?.type === "rejected") {
      this.#projectRejection(outcome)
      return
    }
    if (outcome == null) {
      const presentation = presentError({
        category: "protocol",
        code: "subagent_close_unavailable",
        message: "Couldn't close the child because the engine connection is unavailable.",
      })
      this.#projectClientError(
        "subagent_close_unavailable",
        presentation.text,
        true,
      )
      return
    }
    if (this.#activeSubagentId === subagentId) this.#leaveSubagent()
    const { [subagentId]: _closed, ...subagents } = this.#state.subagents
    this.#state = {
      ...this.#state,
      subagents,
      subagentOrder: this.#state.subagentOrder.filter((candidate) => candidate !== subagentId),
    }
    this.#subagentDescriptors = this.#subagentDescriptors.filter(
      (subagent) => subagent.subagent_id !== subagentId,
    )
    this.#transitionSubagentReplay(subagentId, { type: "close" })
    this.#subagentReplays.delete(subagentId)
    this.#subagentStates.delete(subagentId)
    this.#subagentComposerDrafts.delete(subagentId)
    this.setState(this.#state)
    this.#requestSubagents()
  }

  #requestModels(refresh = false): void {
    this.#modelsRequested = true
    this.#clearProjectionError("models")
    this.#projectionRequests.command({ type: "list_models", refresh })
  }

  #requestModes(): void {
    this.#clearProjectionError("modes")
    this.#projectionRequests.command({ type: "list_modes" })
  }

  #openChangedFileDiff(path: string): void {
    if (this.#state.replay.active || this.#state.shell.active) return
    this.#reviewOpen = true
    this.reviewPanel.showWorkspaceDiffMessage(path, "Loading changed-file diff…")
    this.setState(this.#state)
    this.#projectionRequests.command({
      type: "get_workspace_diff",
      path,
      max_bytes: 1_000_000,
    })
  }

  #scheduleSessionSearch(query: string): void {
    this.#clearSessionSearchTimer()
    this.#sessionSearchTimer = setTimeout(() => {
      this.#sessionSearchTimer = null
      if (this.#pickerController.kind === "sessions" && this.picker.input.value === query) {
        if (query.trim().length === 0) {
          this.#projectionRequests.command({ type: "list_sessions" })
        } else {
          this.#projectionRequests.command({ type: "search_sessions", query, limit: 100 })
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
      this.#pendingRewindIntent = null
      this.#clearComposerNotice()
    }
    if (this.#state.replay.active) {
      return false
    }
    if (content.startsWith("!")) {
      const originatingSubagentId = this.#activeSubagentId
      const accepted = await this.#startForegroundShell(content, attachments)
      if (accepted && originatingSubagentId !== null && this.#activeSubagentId === originatingSubagentId) {
        this.#leaveSubagent()
      }
      return accepted
    }
    if (this.#activeSubagentId !== null) {
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
      const subagentId = this.#activeSubagentId
      if (this.#subagentDescriptor(subagentId)?.activity === "running") {
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
      this.#subagentErrorBaseline = this.#state.errors.at(-1)
      this.#setSubagentActivity(subagentId, "running")
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
    if (preserveRewindIntent && this.#pendingRewindIntent !== null) {
      this.#pendingRewindIntent.requestId = meta.request_id
    }
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

  async #submitProviderApiKey(provider: string, apiKey: string): Promise<void> {
    this.#providerApiKeyPending = provider
    this.#pickerController.kind = "providerApiKey"
    this.#pickerController.refresh()
    try {
      const result = await this.#options.onProviderApiKey?.(provider, apiKey)
      if (result === undefined)
        throw new Error("credential transport unavailable")
      this.#requestModels(true)
      if (result.activated) {
        this.#storedProviderKeys.delete(provider)
      } else {
        if (this.#storedProviderKeys.size >= 32) {
          const oldest = this.#storedProviderKeys.values().next().value
          if (oldest !== undefined) this.#storedProviderKeys.delete(oldest)
        }
        this.#storedProviderKeys.add(provider)
        this.#projectClientError(
          "provider_activation_pending",
          "credential stored securely, but activation is pending; select the provider again to refresh without re-entering the key",
          true
        )
      }
      this.openProviderPicker()
      for (const warning of result.warnings.slice(0, 16)) {
        this.#projectClientError("provider_credential_warning", warning)
      }
    } catch {
      this.#projectClientError(
        "provider_credential_failed",
        "provider credential submission failed; verify the key and try again",
        true
      )
      this.openProviderPicker()
    } finally {
      this.#providerApiKeyPending = null
    }
  }

  async #retryProviderActivation(provider: string): Promise<void> {
    this.#providerApiKeyPending = provider
    this.#pickerController.kind = "providerApiKey"
    this.#pickerController.refresh()
    try {
      if (this.#options.onProviderActivate === undefined) throw new Error("activation unavailable")
      await this.#options.onProviderActivate(provider)
      this.#storedProviderKeys.delete(provider)
      this.#requestModels(true)
      this.openProviderPicker()
    } catch {
      this.#projectClientError(
        "provider_activation_failed",
        "credential remains stored securely, but activation failed; retry from /providers",
        true,
      )
      this.openProviderPicker()
    } finally {
      this.#providerApiKeyPending = null
    }
  }

  async #runProviderAuthAction(
    provider: string,
    attemptId: string,
    action: ProviderAuthPickerAction,
  ): Promise<void> {
    if (this.#providerAuthActionInFlight) return
    const pending = this.#state.providerAuth.pending
    if (
      pending === null ||
      pending.provider !== provider ||
      pending.attemptId !== attemptId
    )
      return
    this.#providerAuthActionInFlight = true
    let failureCode = "provider_auth_action_failed"
    let failureMessage =
      "provider authentication action failed; copy the URL manually"
    try {
      switch (action.kind) {
        case "open_url":
          failureCode = "provider_auth_browser_failed"
          failureMessage =
            "couldn't open a browser; use Copy URL and open it manually"
          await this.#options.externalUrl.open(action.value)
          this.#providerAuthActionNotice =
            "Browser opened · waiting for authentication"
          break
        case "copy_code":
          failureCode = "provider_auth_copy_failed"
          failureMessage =
            "couldn't copy the device code; enter the displayed code manually"
          await this.#options.textClipboard.writeText(action.value)
          this.#providerAuthActionNotice =
            "Code copied · waiting for authentication"
          break
        case "copy_url":
          failureCode = "provider_auth_copy_failed"
          failureMessage =
            "couldn't copy the URL; open the displayed URL manually"
          await this.#options.textClipboard.writeText(action.value)
          this.#providerAuthActionNotice =
            "URL copied · waiting for authentication"
          break
        case "cancel":
          return
      }
    } catch {
      this.#providerAuthActionNotice = null
      this.#projectClientError(failureCode, failureMessage, true)
    } finally {
      this.#providerAuthActionInFlight = false
      const current = this.#state.providerAuth.pending
      if (
        this.#pickerController.kind === "providerAuth" &&
        current?.provider === provider &&
        current.attemptId === attemptId
      ) {
        this.#pickerController.refresh()
      }
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
    this.#focusForInputMode()
  }

  #closeOutputViewer(): void {
    if (this.#outputViewerToolCallId === null) return
    this.#outputViewerToolCallId = null
    this.outputViewer.closePresentation()
    this.setState(this.#state)
    this.#focusForInputMode()
  }

  #recordProjectionFailure(kind: ProjectionKind, message: string): void {
    if (kind === "commands") {
      this.#commandsRequested = false
    } else if (kind === "models") {
      this.#modelsRequested = false
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

  #isInterruptible(): boolean {
    return this.#interruptSubagentId !== null ||
      this.#state.compaction.active ||
      Object.values(this.#state.turns).some((turn) => turn.status === "running")
  }

  #armInterruptEscape(subagentId: string | null = null): void {
    this.#clearInterruptEscape(false)
    this.#interruptEscapeArmed = true
    this.#interruptSubagentId = subagentId
    this.banner.visible = true
    this.banner.fg = this.#theme.warning
    this.banner.content = subagentId === null
      ? "Press Esc again to stop the active response"
      : "Back in parent · press Esc again to stop the child agent"
    this.#interruptEscapeTimer = setTimeout(() => this.#clearInterruptEscape(), 900)
  }

  #clearInterruptEscape(refresh = true): void {
    if (this.#interruptEscapeTimer !== null) {
      clearTimeout(this.#interruptEscapeTimer)
      this.#interruptEscapeTimer = null
    }
    if (!this.#interruptEscapeArmed) return
    this.#interruptEscapeArmed = false
    this.#interruptSubagentId = null
    if (refresh && !this.#destroyed) this.banner.update(this.#state)
  }

  async #interruptActiveResponse(subagentId: string | null = this.#interruptSubagentId): Promise<void> {
    if (subagentId !== null) {
      this.#interruptSubagentId = null
      await this.#interruptSubagent(subagentId)
      return
    }
    const outcome = await this.#projectionRequests.emit({
      type: "interrupt",
      meta: this.#projectionRequests.meta(),
      session_id: this.#sessionId,
    })
    if (outcome === null) {
      this.#projectClientError(
        "interrupt_unavailable",
        "Couldn't stop the active response because the engine connection is unavailable.",
        true,
      )
      return
    }
    this.#projectRejection(outcome)
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

function timelineUserMessage(turn: RottweilerState["transcript"][number]["turn"]): {
  readonly content: string
  readonly hadAttachments: boolean
} {
  const first = turn.blocks[0]
  if (first?.type !== "text") {
    return { content: "", hadAttachments: turn.blocks.length > 0 }
  }
  const firstIsTextAttachment = /^Attached file .+ \([^\n]+\):\n/.test(first.text)
  return {
    content: firstIsTextAttachment ? "" : first.text,
    hadAttachments: firstIsTextAttachment || turn.blocks.length > 1,
  }
}

function safeErrorMessage(error: unknown): string {
  return error instanceof Error && error.message.length > 0
    ? error.message
    : "the request could not be delivered to the engine"
}

function expandLeadingHome(path: string): string {
  if (path === "~") return homedir()
  return path.startsWith("~/") ? `${homedir()}${path.slice(1)}` : path
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

const IMMEDIATE_PRESENTATION_EVENTS = new Set<WireEngineEvent["type"]>([
  "command_acknowledged",
  "context_snapshot_ready",
  "cost_snapshot_ready",
  "session_review_ready",
  "session_review_updated",
  "prompt_dump_ready",
  "session_replay_completed",
  "session_forked",
  "session_exported",
  "sessions_listed",
  "subagents_listed",
  "subagent_replay_batch",
  "subagent_replay_completed",
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
  event: { readonly type: WireEngineEvent["type"] },
): boolean {
  return !IMMEDIATE_PRESENTATION_EVENTS.has(event.type)
}

/** Build the retained OpenTUI application tree. */
export function createRottweilerApp(
  renderer: RenderContext,
  options: RottweilerAppOptions = {},
): RottweilerApp {
  return new RottweilerApp(renderer, options)
}
