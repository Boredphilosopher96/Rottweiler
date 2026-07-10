import {
  BoxRenderable,
  CliRenderEvents,
  type RenderContext,
  type TreeSitterClient,
} from "@opentui/core"

import {
  ComposerRenderable,
  ContextPanelRenderable,
  FuzzyPickerRenderable,
  InteractionPanelRenderable,
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
  type Attachment,
  type ClientCommand,
  type EngineEvent,
} from "./protocol"
import {
  createInitialState,
  engineEvent,
  reduceRottweilerState,
  type QuestionProjection,
  type RottweilerState,
  type ToolProjection,
} from "./state"
import { createSyntaxStyle, kennelTheme, type RottweilerTheme } from "./theme"

export interface RottweilerAppOptions {
  readonly initialEvent?: EngineEvent
  readonly initialState?: RottweilerState
  readonly sessionId?: string
  readonly clientId?: string
  readonly onCommand?: (command: ClientCommand) => void | Promise<void>
  readonly requestId?: () => string
  readonly theme?: RottweilerTheme
  readonly treeSitterClient?: TreeSitterClient
  readonly notifications?: NotificationAdapter
  readonly editor?: EditorAdapter
  readonly imagePaste?: ImagePasteAdapter
}

type PickerKind = "commands" | "files" | "modes" | "models" | "sessions"

export class RottweilerApp extends BoxRenderable {
  readonly transcript: TranscriptRenderable
  readonly contextPanel: ContextPanelRenderable
  readonly interactionPanel: InteractionPanelRenderable
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
  #terminalFocused = true
  #pickerKind: PickerKind | null = null
  #pendingFilePreview: string | null = null
  #onTerminalFocus = () => {
    this.#terminalFocused = true
  }
  #onTerminalBlur = () => {
    this.#terminalFocused = false
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
    this.#state = options.initialState ?? createInitialState()
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
      },
      options.treeSitterClient,
    )
    this.picker = new FuzzyPickerRenderable(ctx, theme)
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
      if (key.name === "escape" && this.picker.visible) {
        key.preventDefault()
        this.closePicker()
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
    this.add(this.interactionPanel)
    this.add(this.composer)
    this.add(this.statusLine)
    this.add(this.picker)
    ctx.on(CliRenderEvents.FOCUS, this.#onTerminalFocus)
    ctx.on(CliRenderEvents.BLUR, this.#onTerminalBlur)
    this.setState(this.#state)
    this.composer.focus()
  }

  get state(): RottweilerState {
    return this.#state
  }

  handleEvent(event: EngineEvent): void {
    const previous = this.#state
    const next = reduceRottweilerState(previous, engineEvent(event))
    this.setState(next)
    this.#notify(previous, next)
    if (event.type === "workspace_file_preview_ready" && this.#pendingFilePreview !== null) {
      const preview = event.preview
      this.composer.addAttachment({
        name: preview.path,
        media_type: preview.media_type,
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
    this.contextPanel.visible = this.width === 0 ? this.ctx.width >= 100 : this.width >= 100
    this.interactionPanel.update(state)
    this.composer.setQueuedMessages(state.queuedMessages)
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
    if (this.#state.sessions.length === 0) {
      this.#command({ type: "list_sessions" })
    }
    this.#refreshPicker()
  }

  closePicker(): void {
    this.#pickerKind = null
    this.picker.close()
    this.composer.focus()
  }

  protected override onResize(width: number, _height: number): void {
    this.contextPanel.visible = width >= 100
  }

  override destroy(): void {
    this.ctx.off(CliRenderEvents.FOCUS, this.#onTerminalFocus)
    this.ctx.off(CliRenderEvents.BLUR, this.#onTerminalBlur)
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
              session_id: this.#options.sessionId,
              mode: item.value as string,
            })
            this.closePicker()
          },
        )
        break
      case "sessions":
        this.#openPicker(
          "Sessions",
          this.#state.sessions.map((session) => ({
            id: session.sessionId,
            label: session.workspaceName,
            description: `${session.model}${session.shellActive ? " · shell active" : ""}`,
            value: session,
          })),
          (item) => {
            const session = item.value as RottweilerState["sessions"][number]
            this.#emit({
              type: "attach_session",
              meta: this.#meta(),
              session_id: session.sessionId,
              last_seen_sequence: null,
              role: "driver",
            })
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
    this.picker.open(title, items as readonly PickerItem<unknown>[], (item) =>
      onSelect(item as PickerItem<T>),
    )
  }

  #sendMessage(content: string, attachments: readonly Attachment[]): void {
    this.#emit({
      type: "send_message",
      meta: this.#meta(),
      session_id: this.#options.sessionId,
      content,
      attachments: [...attachments],
    })
  }

  #approve(tool: ToolProjection, decision: ApprovalDecision): void {
    this.#emit({
      type: "approve_tool",
      meta: this.#meta(),
      session_id: this.#options.sessionId,
      tool_call_id: tool.toolCallId,
      decision,
    })
  }

  #answer(question: QuestionProjection, values: readonly string[]): void {
    this.#emit({
      type: "answer_question",
      meta: this.#meta(),
      session_id: this.#options.sessionId,
      question_id: question.questionId,
      answers: [{ question_id: question.questionId, values: [...values] }],
    })
  }

  #command(
    command:
      | { readonly type: "pin_context"; readonly item_id: string }
      | { readonly type: "evict_context"; readonly item_id: string }
      | { readonly type: "search_workspace_files"; readonly query: string; readonly limit: number }
      | { readonly type: "preview_workspace_file"; readonly path: string; readonly max_bytes: number }
      | { readonly type: "switch_model"; readonly model: string }
      | { readonly type: "list_commands" | "list_models" | "list_sessions" },
  ): void {
    const meta = this.#meta()
    switch (command.type) {
      case "list_commands":
      case "list_models":
      case "list_sessions":
        this.#emit({ type: command.type, meta })
        break
      case "pin_context":
      case "evict_context":
        this.#emit({
          type: command.type,
          meta,
          session_id: this.#options.sessionId,
          item_id: command.item_id,
        })
        break
      case "search_workspace_files":
        this.#emit({
          ...command,
          meta,
          session_id: this.#options.sessionId,
        })
        break
      case "preview_workspace_file":
        this.#emit({
          ...command,
          meta,
          session_id: this.#options.sessionId,
        })
        break
      case "switch_model":
        this.#emit({
          ...command,
          meta,
          session_id: this.#options.sessionId,
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

  #emit(command: ClientCommand): void {
    void this.#options.onCommand?.(command)
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
    if (approval !== undefined) {
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

/** Build the retained OpenTUI application tree. */
export function createRottweilerApp(
  renderer: RenderContext,
  options: RottweilerAppOptions = {},
): RottweilerApp {
  return new RottweilerApp(renderer, options)
}
