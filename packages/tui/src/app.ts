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
import { isSessionForkedEvent, type WireEngineEvent } from "./transport"

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
}

export interface TerminalHandoverAdapter {
  suspend(): void
  resume(): void
}

type PickerKind = "commands" | "files" | "modes" | "models" | "sessions"

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
  #pendingFilePreview: string | null = null
  #terminalSuspended = false
  #pendingShellTimer: ReturnType<typeof setTimeout> | null = null
  #pluginNotificationTimer: ReturnType<typeof setTimeout> | null = null
  #sessionSearchTimer: ReturnType<typeof setTimeout> | null = null
  #pendingForkRequests = new Set<string>()
  #pendingReviewPaths = new Set<string>()
  #onTerminalFocus = () => {
    this.#terminalFocused = true
  }
  #onTerminalBlur = () => {
    this.#terminalFocused = false
  }
  #onGlobalKey = (key: KeyEvent) => {
    if (
      (key.name === "escape" || key.name === "esc") &&
      this.#state.review !== null &&
      !this.picker.visible
    ) {
      key.preventDefault()
      this.#closeReview()
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
    this.#syntaxStyle = createSyntaxStyle(theme)
    this.#sessionId = this.#options.sessionId
    const initialState = options.initialState ?? createInitialState()
    this.#state =
      options.replaySessionId === undefined
        ? initialState
        : enterReplayMode(initialState, options.replaySessionId)
    if (options.initialEvent !== undefined) {
      this.#state = reduceRottweilerState(this.#state, engineEvent(options.initialEvent))
    }

    this.banner = new StateBannerRenderable(ctx, theme)
    this.main = new BoxRenderable(ctx, {
      id: "main-content",
      width: "100%",
      flexGrow: 1,
      minHeight: 5,
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
      onPin: (itemId) => this.#command({ type: "pin_context", item_id: itemId }),
      onEvict: (itemId) => this.#command({ type: "evict_context", item_id: itemId }),
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
      onFileMention: (query) => this.openFilePicker(query),
    })
    this.statusLine = new StatusLineRenderable(ctx, theme)
    this.onKeyDown = (key) => {
      if ((key.name === "escape" || key.name === "esc") && this.picker.visible) {
        key.preventDefault()
        this.closePicker()
      } else if (
        (key.name === "escape" || key.name === "esc") &&
        this.#state.review !== null
      ) {
        key.preventDefault()
        this.#closeReview()
      } else if (this.#state.replay.active && key.ctrl && key.name === "s") {
        key.preventDefault()
        this.openSessionPicker()
      } else if (this.#state.replay.active) {
        return
      } else if (key.ctrl && key.name === "r") {
        key.preventDefault()
        this.openReview()
      } else if (key.shift && key.name === "tab") {
        key.preventDefault()
        const current = this.#state.mode ?? "execute"
        const mode: ModeId = current === "execute" ? "discuss" : current === "discuss" ? "plan" : "execute"
        this.#emit({
          type: "switch_mode",
          meta: this.#meta(),
          session_id: this.#sessionId,
          mode,
        })
      } else if (key.ctrl && key.name === "p") {
        key.preventDefault()
        this.openCommandPicker()
      } else if (key.ctrl && key.name === "m") {
        key.preventDefault()
        this.openModelPicker()
      } else if (key.ctrl && key.name === "o") {
        key.preventDefault()
        this.openModePicker()
      } else if (key.ctrl && key.name === "s") {
        key.preventDefault()
        this.openSessionPicker()
      } else if (key.ctrl && key.name === "i") {
        key.preventDefault()
        void this.composer.pasteImage()
      }
    }

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
    if (this.#state.review !== null) {
      this.reviewPanel.files.focus()
    } else if (!this.#state.replay.active) {
      this.composer.focus()
    }
  }

  get state(): RottweilerState {
    return this.#state
  }

  /** Update command routing only after the runtime owns the new driver lease. */
  setSessionId(sessionId: string): void {
    this.#sessionId = sessionId
    if (this.#state.replay.active && this.#state.replay.sessionId !== sessionId) {
      this.setState(enterReplayMode(createInitialState(), sessionId))
    }
  }

  handleEvent(event: WireEngineEvent): void {
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
    const previous = this.#state
    const next = reduceRottweilerState(previous, engineEvent(event))
    this.setState(next)
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
  }

  setState(state: RottweilerState): void {
    this.#state = state
    this.transcript.update(state)
    this.contextPanel.update(state)
    this.contextPanel.visible =
      !state.replay.active && (this.width === 0 ? this.ctx.width >= 100 : this.width >= 100)
    this.interactionPanel.update(state)
    this.reviewPanel.update(state)
    this.composer.setQueuedMessages(state.queuedMessages)
    this.composer.visible = !state.replay.active && state.review === null
    this.statusLine.setBranch(state.workspaceStatus?.branch ?? null)
    this.statusLine.update(state)
    this.banner.update(state)
    if (this.#pickerKind !== null) {
      this.#refreshPicker()
    }
  }

  openCommandPicker(): void {
    this.#pickerKind = "commands"
    if (this.#state.commands.length === 0) {
      this.#command({ type: "list_commands" })
    }
    this.#refreshPicker()
  }

  openFilePicker(query = ""): void {
    this.#pickerKind = "files"
    this.#command({ type: "search_workspace_files", query, limit: 100 })
    this.#refreshPicker()
    this.picker.input.value = query
  }

  openModelPicker(): void {
    this.#pickerKind = "models"
    if (this.#state.models.length === 0) {
      this.#command({ type: "list_models" })
    }
    this.#refreshPicker()
  }

  openModePicker(): void {
    this.#pickerKind = "modes"
    this.#refreshPicker()
  }

  openSessionPicker(): void {
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
    this.#command({ type: "get_session_review" })
  }

  closePicker(): void {
    this.#pickerKind = null
    this.picker.close()
    if (!this.#state.replay.active) {
      this.composer.focus()
    }
  }

  protected override onResize(width: number, _height: number): void {
    this.contextPanel.visible = !this.#state.replay.active && width >= 100
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

  #refreshPicker(): void {
    switch (this.#pickerKind) {
      case "commands":
        this.#openPicker(
          "Commands",
          this.#state.commands.map((command) => ({
            id: command.name,
            label: `/${command.name}`,
            description: command.description,
            searchText: command.usage,
            value: command,
          })),
          (item) => {
            const command = item.value as RottweilerState["commands"][number]
            if (command.name === "review") {
              this.openReview()
              this.closePicker()
              return
            }
            if (command.name === "fork") {
              void this.#requestFork(null)
              this.closePicker()
              return
            }
            this.composer.value = `/${command.name} `
            this.closePicker()
          },
        )
        break
      case "files":
        this.#openPicker(
          "Workspace files",
          this.#state.workspaceFiles.map((file) => ({
            id: file.path,
            label: file.isDirectory ? `▸ ${file.path}` : file.path,
            description: file.isDirectory ? "directory" : "attach file",
            value: file,
          })),
          (item) => {
            const file = item.value as RottweilerState["workspaceFiles"][number]
            if (!file.isDirectory) {
              this.#pendingFilePreview = file.path
              this.#command({ type: "preview_workspace_file", path: file.path, max_bytes: 1_000_000 })
            }
          },
        )
        break
      case "models":
        this.#openPicker(
          "Models",
          this.#state.models.map((model) => ({
            id: model.alias,
            label: model.alias,
            description: [model.toolCalling ? "tools" : "", model.vision ? "vision" : "", model.thinking ? "thinking" : ""]
              .filter(Boolean)
              .join(" · "),
            value: model,
          })),
          (item) => {
            const model = item.value as RottweilerState["models"][number]
            this.#command({ type: "switch_model", model: model.alias })
            this.closePicker()
          },
        )
        break
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
        this.#openPicker(
          this.#state.sessionSearch?.truncated === true
            ? "Sessions · results truncated"
            : "Sessions",
          this.#state.sessions.map((session) => ({
            id: session.sessionId,
            label: session.workspaceName,
            description: `${session.model}${session.shellActive ? " · shell active" : ""}`,
            searchText: `${session.sessionId} ${session.workspaceName} ${session.model}`,
            value: session,
          })),
          (item) => {
            const session = item.value as RottweilerState["sessions"][number]
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
    this.picker.refresh(title, items as readonly PickerItem<unknown>[], (item) =>
      onSelect(item as PickerItem<T>),
    )
  }

  #scheduleSessionSearch(query: string): void {
    this.#clearSessionSearchTimer()
    this.#sessionSearchTimer = setTimeout(() => {
      this.#sessionSearchTimer = null
      if (this.#pickerKind === "sessions" && this.picker.input.value === query) {
        if (query.trim().length === 0) {
          this.#command({ type: "list_sessions" })
        } else {
          this.#command({ type: "search_sessions", query, limit: 100 })
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
    if (sessionAction?.type === "review") {
      if (this.#state.shell.active) {
        this.#projectClientError(
          "review_unavailable_during_shell",
          "exit the foreground shell before opening session review",
        )
        return false
      }
      const outcome = await this.#emit({
        type: "get_session_review",
        meta: this.#meta(),
        session_id: this.#sessionId,
      })
      if (outcome?.type !== "accepted") this.#projectRejection(outcome)
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
    this.#emit({
      type: "approve_tool",
      meta: this.#meta(),
      session_id: this.#sessionId,
      tool_call_id: tool.toolCallId,
      decision,
      binding: approvalBinding(tool.diff),
    })
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
    if (this.#state.review === null) return
    this.setState({ ...this.#state, review: null })
    this.composer.focus()
  }

  #command(
    command:
      | { readonly type: "pin_context"; readonly item_id: string }
      | { readonly type: "evict_context"; readonly item_id: string }
      | { readonly type: "search_workspace_files"; readonly query: string; readonly limit: number }
      | { readonly type: "preview_workspace_file"; readonly path: string; readonly max_bytes: number }
      | { readonly type: "switch_model"; readonly model: string }
      | { readonly type: "get_session_review" }
      | { readonly type: "search_sessions"; readonly query: string; readonly limit: number }
      | { readonly type: "list_commands" | "list_models" | "list_sessions" },
  ): void {
    if (
      this.#state.replay.active &&
      command.type !== "list_sessions" &&
      command.type !== "search_sessions"
    ) {
      return
    }
    const meta = this.#meta()
    switch (command.type) {
      case "list_commands":
      case "list_models":
      case "list_sessions":
        this.#emit({ type: command.type, meta })
        break
      case "search_sessions":
        this.#emit({ ...command, meta })
        break
      case "get_session_review":
        this.#emit({ type: command.type, meta, session_id: this.#sessionId })
        break
      case "pin_context":
      case "evict_context":
        this.#emit({
          type: command.type,
          meta,
          session_id: this.#sessionId,
          item_id: command.item_id,
        })
        break
      case "search_workspace_files":
        this.#emit({
          ...command,
          meta,
          session_id: this.#sessionId,
        })
        break
      case "preview_workspace_file":
        this.#emit({
          ...command,
          meta,
          session_id: this.#sessionId,
        })
        break
      case "switch_model":
        this.#emit({
          ...command,
          meta,
          session_id: this.#sessionId,
        })
        break
    }
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
  | { readonly type: "invalid"; readonly message: string }

function parseSessionAction(content: string): SessionAction | null {
  const tokens = content.trim().split(/\s+/)
  const command = tokens[0]
  if (command === "/review") {
    return tokens.length === 1
      ? { type: "review" }
      : { type: "invalid", message: "usage: /review" }
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
