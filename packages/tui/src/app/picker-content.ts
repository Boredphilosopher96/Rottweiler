import type { RottweilerApp } from "../app"
import {
  createCommandPaletteModel,
  type CommandPaletteEntry,
  type CommandPaletteCatalog,
} from "../command-palette"
import type { ListDetailPresentation, PickerItem } from "../components"
import {
  KEYBINDING_ACTION_LABELS,
  formatKeycap,
  type KeybindingAction,
  type KeybindingContext,
  type KeybindingPreset,
} from "../keybindings"
import type { PickerController } from "../picker-controller"
import type { ProjectionKind, ProjectionRequestBroker } from "../projection-requests"
import type { Attachment } from "../protocol"
import {
  commandSourceLabel,
  isTuiHandledSlashCommand,
  mergeSlashCommandChoices,
  type CommandChoice,
} from "../session-commands"
import type { RottweilerState } from "../state"
import { modePickerPresentation } from "../ui-presentation"
import type { InputUiController } from "./input"
import type { ChildUiController } from "./children"
import type { SessionUiController } from "./sessions"
import type { ProviderUiController } from "./provider"
import type { PermissionUiController } from "./permissions"
import type { SettingsUiController } from "./settings"
import type { McpUiController } from "./mcp"
import type { ThemeUiController } from "./themes"
interface PickerContentHost {
  readonly ui: Pick<RottweilerApp,
    | "picker"
    | "closePicker"
    | "commandPalette"
    | "composer"
    | "openBudgetPicker"
    | "openExportSessionPicker"
    | "openMcpPicker"
    | "openModelPicker"
    | "openPermissionModePicker"
    | "openPermissionPicker"
    | "openProviderPicker"
    | "openQueuedMessagesPicker"
    | "openReview"
    | "openSessionPicker"
    | "openSettingsPicker"
    | "openSubagentActionPicker"
    | "openSubagentPicker"
    | "openThemePicker"
    | "openTimelinePicker"
    | "openTrustPicker"
    | "showConversationView"
    | "showToolsView"
    | "state"
  >
  readonly pickerController: PickerController
  readonly input: InputUiController
  readonly requests: ProjectionRequestBroker
  readonly projectionErrors: Partial<Record<ProjectionKind, string>>
  readonly terminalWidth: number
  readonly terminalHeight: number
  readonly sessionId: string
  readonly children: ChildUiController
  readonly sessions: SessionUiController
  readonly providers: ProviderUiController
  readonly permissions: PermissionUiController
  readonly settings: SettingsUiController
  readonly mcp: McpUiController
  readonly themes: ThemeUiController
  onExit(): void
  modalOpened(): void
  clearProjectionError(kind: ProjectionKind): void
  requestFork(atTurn: string | null): Promise<boolean>
  sendMessage(content: string, attachments: readonly Attachment[]): Promise<boolean>
}
export interface PaletteAction {
  readonly id: string
  readonly title: string
  readonly description: string
  readonly section: PaletteSection
  readonly catalogSource?: "builtin" | "extension"
  readonly sourceLabel?: string
  readonly detailDescription?: string
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

export class PickerContentController {
  #commandsRequested = false
  constructor(readonly host: PickerContentHost) {}
  get commandsRequested(): boolean { return this.#commandsRequested }
  resetCommands(): void { this.#commandsRequested = false }
  renderPicker(): void {
    switch (this.host.pickerController.kind) {
      case "palette": {
        const paletteActions = this.paletteActions()
        const entries: readonly CommandPaletteEntry<PaletteAction>[] = paletteActions.map((action) => ({
            id: action.id,
            title: action.title,
            description: action.description,
            section: action.section,
            source: action.catalogSource ?? "builtin",
            action,
          }))
        const catalog: CommandPaletteCatalog = this.host.projectionErrors.commands !== undefined
          ? {
              kind: "error",
              message: this.host.projectionErrors.commands,
              retryable: true,
            }
          : this.#commandsRequested && this.host.ui.state.commands.length === 0
            ? { kind: "loading" }
            : { kind: "ready", truncated: this.host.ui.state.commandsTruncated }
        const query = this.host.ui.commandPalette.visible
          ? this.host.ui.commandPalette.input.value
          : this.host.pickerController.query
        const preserveSelection = query === this.host.pickerController.query
        this.host.pickerController.query = query
        const model = createCommandPaletteModel({
          entries,
          sections: PALETTE_SECTIONS,
          query,
          selectedId: this.host.ui.commandPalette.visible && preserveSelection
            ? this.host.ui.commandPalette.selectedId
            : null,
          catalog,
        })
        const presentation: ListDetailPresentation<PaletteAction> = {
          title: "COMMAND PALETTE",
          query,
          selectedId: model.selectedId,
          rows: model.rows.map((row) => row.kind === "section"
            ? row
            : {
                kind: "item",
                id: row.id,
                label: row.title,
                matchSpans: row.titleMatches,
                detail: {
                  title: row.title,
                  description: row.action.detailDescription ?? row.description,
                  meta: `${row.section} · ${row.action.sourceLabel ?? (row.source === "builtin" ? "built-in" : "extension")}`,
                },
                action: row.action,
              }),
          status: model.status,
          notice: model.notice === null
            ? null
            : {
                message: model.notice.kind === "error" && model.notice.retryable
                  ? `${model.notice.message} · Ctrl+R retry`
                  : model.notice.message,
                tone: model.notice.kind === "error"
                  ? "error"
                  : model.notice.kind === "truncated"
                    ? "warning"
                    : "muted",
              },
        }
        if (this.host.ui.commandPalette.visible) {
          this.host.ui.commandPalette.refresh(presentation)
        } else {
          this.host.ui.commandPalette.open(presentation, (action) => action.run(), {
            onQuery: () => {
              this.renderPicker()
            },
            onRetry: () => {
              this.requestCommands()
              this.renderPicker()
            },
          })
          this.host.modalOpened()
        }
        break
      }
      case "keyboardHelp": {
        const items: PickerItem<null>[] = []
        for (const context of KEYBOARD_HELP_CONTEXTS[this.host.input.bindings.preset]) {
          const bindings = this.host.input.bindings.bindings(context)
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
        this.host.pickerController.show("Keyboard shortcuts", items, () => this.host.ui.closePicker())
        break
      }
      case "commands":
        const commandError = this.host.projectionErrors.commands
        const commandItems: PickerItem<CommandChoice | null>[] = [
          ...(commandError === undefined
            ? []
            : [{
                id: "commands.error",
                label: "Couldn't load live commands",
                description: `${commandError} · select to retry`,
                value: null,
              }]),
          ...mergeSlashCommandChoices(this.host.ui.state.commands).map((command) => ({
            id: command.name,
            label: `/${command.name}`,
            description: `${commandSourceLabel(command.source)} · ${command.description}`,
            searchText: command.usage,
            value: command,
          })),
        ]
        this.host.pickerController.show(
          this.host.ui.state.commandsTruncated ? "Commands · results truncated" : "Commands",
          commandItems,
          (item) => {
            const command = item.value as CommandChoice | null
            if (command === null) {
              this.requestCommands()
              return
            }
            const clearAnchoredTrigger = () => {
              if (this.host.pickerController.anchored) this.host.ui.composer.value = ""
            }
            if (command.name === "review") {
              clearAnchoredTrigger()
              this.host.ui.openReview()
              this.host.ui.closePicker()
              return
            }
            if (command.name === "fork") {
              clearAnchoredTrigger()
              void this.host.requestFork(null)
              this.host.ui.closePicker()
              return
            }
            if (command.name === "rewind") {
              clearAnchoredTrigger()
              this.host.ui.closePicker()
              this.host.ui.openTimelinePicker()
              return
            }
            if (command.name === "models") {
              clearAnchoredTrigger()
              this.host.ui.closePicker()
              this.host.ui.openModelPicker()
              return
            }
            if (command.name === "providers") {
              clearAnchoredTrigger()
              this.host.ui.closePicker()
              this.host.ui.openProviderPicker()
              return
            }
            if (command.name === "agents") {
              clearAnchoredTrigger()
              this.host.ui.closePicker()
              this.host.ui.openSubagentPicker()
              return
            }
            if (command.name === "theme") {
              clearAnchoredTrigger()
              this.host.ui.closePicker()
              this.host.ui.openThemePicker()
              return
            }
            if (command.name === "settings") {
              clearAnchoredTrigger()
              this.host.ui.closePicker()
              this.host.ui.openSettingsPicker()
              return
            }
            if (command.name === "mode") {
              clearAnchoredTrigger()
              this.host.ui.closePicker()
              this.openModePicker()
              return
            }
            const content = `/${command.name}`
            const requiresArgument = /<[^>]+>/.test(command.usage)
            if (this.host.pickerController.anchored && !requiresArgument) {
              this.host.ui.composer.value = content
              this.host.ui.closePicker()
              void this.host.ui.composer.submit()
              return
            }
            this.host.ui.composer.value = `${content} `
            this.host.ui.closePicker()
          },
        )
        break
      case "timeline": this.host.sessions.render("timeline"); break
      case "timelineActions": this.host.sessions.render("timelineActions"); break
      case "queuedMessages": this.host.sessions.render("queuedMessages"); break
      case "exportFormat": this.host.sessions.render("exportFormat"); break
      case "exportOverwrite": this.host.sessions.render("exportOverwrite"); break
      case "exportPath": this.host.sessions.render("exportPath"); break
      case "workspaceRoots": {
        const workspaceRoots = this.host.ui.state.workspaceRoots
        if (workspaceRoots === null) {
          this.host.pickerController.showLoading("Workspace roots", "Loading workspace roots")
          break
        }
        this.host.pickerController.show(
          "Workspace roots",
          workspaceRoots.roots.map((root, index) => ({
            id: `workspace.root.${index}`,
            label: root,
            description: index === 0 ? "primary" : "additional",
            value: root,
          })),
          () => this.host.ui.closePicker(),
        )
        break
      }
      case "files":
        const fileError = this.host.projectionErrors.files
        if (
          fileError === undefined &&
          this.host.requests.current("files") !== null &&
          this.host.ui.state.workspaceFiles.length === 0
        ) {
          this.host.pickerController.showLoading("Workspace files", "Searching workspace files")
          break
        }
        if (fileError === undefined && this.host.ui.state.workspaceFiles.length === 0) {
          this.host.pickerController.showStatus(
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
          ...this.host.ui.state.workspaceFiles.map((file) => ({
            id: file.path,
            label: file.isDirectory ? `▸ ${file.path}` : file.path,
            description: file.isDirectory ? "directory" : "attach file",
            value: file,
          })),
        ]
        this.host.pickerController.show(
          "Workspace files",
          fileItems,
          (item) => {
            const file = item.value as RottweilerState["workspaceFiles"][number] | null
            if (file === null) {
              this.openFilePicker(this.host.pickerController.query, this.host.pickerController.anchored)
              return
            }
            if (file.isDirectory) {
              const query = `${file.path.replace(/\/$/, "")}/`
              if (this.host.pickerController.anchored) {
                const mention = this.host.ui.composer.currentFileMention()
                if (mention !== null) {
                  this.host.ui.composer.replaceRange(mention.start, mention.end, `@${query}`)
                }
              } else {
                this.openFilePicker(query)
              }
              return
            }
            const draft = this.host.ui.composer.value
            const mention = this.host.pickerController.anchored ? this.host.ui.composer.currentFileMention() : null
            const requestId = this.host.requests.command({
              type: "preview_workspace_file",
              path: file.path,
              max_bytes: 5_242_880,
            })
            if (requestId !== null) {
              this.host.requests.setFilePreview({
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
        const attachments = this.host.ui.composer.attachments
        const items: PickerItem<number>[] = attachments.map((attachment, index) => ({
          id: `attachment:${index}`,
          label: `Remove ${attachment.source_path ?? attachment.name}`,
          description: `${attachment.media_type} · remove only this attachment`,
          value: index,
        }))
        if (items.length === 0) {
          this.host.pickerController.showStatus(
            "Attachments",
            "No attachments in this draft",
            "Paste an image or select a file with @ to attach it.",
          )
          break
        }
        this.host.pickerController.show("Attachments", items, (item) => {
          this.host.ui.composer.removeAttachment(item.value as number)
          if (this.host.ui.composer.attachments.length === 0) this.host.ui.closePicker()
          else this.host.pickerController.refresh()
        })
        break
      }
      case "models":
      case "providers":
      case "providerRecovery":
      case "providerAuth":
      case "providerApiKey":
        this.host.providers.render(this.host.pickerController.kind)
        break
      case "permissionInput":
      case "trust":
      case "permissionMode":
      case "permissionYoloConfirm":
      case "permissions":
        this.host.permissions.render(this.host.pickerController.kind)
        break

      case "mcp":
      case "mcpInput":
      case "mcpActions":
      case "mcpRemoveConfirm":
        this.host.mcp.render(this.host.pickerController.kind)
        break
      case "budgets":
      case "budgetPresets":
      case "settings":
      case "settingChoices":
        this.host.settings.render(this.host.pickerController.kind)
        break
      case "themes": this.host.themes.render(); break

      case "modes": {
        const presentation = modePickerPresentation(
          this.host.ui.state,
          this.host.projectionErrors.modes,
          this.host.requests.current("modes") !== null,
        )
        this.host.pickerController.show(
          presentation.title,
          presentation.items,
          (item) => {
            if (item.value.kind === "retry") {
              this.requestModes()
              this.host.pickerController.refresh()
              return
            }
            this.host.requests.emit({
              type: "switch_mode",
              meta: this.host.requests.meta(),
              session_id: this.host.sessionId,
              mode: item.value.id,
            })
            this.host.ui.closePicker()
          },
        )
        break
      }
      case "agents": this.host.children.render("agents"); break
      case "agentActions": this.host.children.render("agentActions"); break
      case "sessions": this.host.sessions.render("sessions"); break
      case "sessionActions": this.host.sessions.render("sessionActions"); break
      case "sessionRename": this.host.sessions.render("sessionRename"); break
      case null:
        break
    }
  }

  submitPaletteCommand(content: string): void {
    this.host.ui.closePicker()
    if (
      this.host.ui.state.connection.phase === "connected" ||
      this.host.ui.state.connection.phase === "replaying"
    ) {
      void this.host.sendMessage(content, [])
    } else {
      this.host.ui.composer.value = content
      this.host.ui.composer.focus()
    }
  }

  paletteBinding(action: KeybindingAction): string | null {
    return this.bindingHint(action, ["global"])
  }

  bindingHint(action: KeybindingAction, contexts: readonly KeybindingContext[]): string | null {
    for (const context of contexts) {
      for (const [stroke, boundAction] of this.host.input.bindings.bindings(context)) {
        if (boundAction === action) return formatKeycap(stroke)
      }
    }
    return null
  }

  composerKeybindingContext(): Extract<KeybindingContext, "standard" | "vim_insert"> {
    return this.host.input.bindings.preset === "vim" ? "vim_insert" : "standard"
  }

  paletteDescription(description: string, binding?: KeybindingAction): string {
    if (binding === undefined) return description
    const hint = this.paletteBinding(binding)
    return hint === null ? description : `${description} · ${hint}`
  }

  paletteActions(): readonly PaletteAction[] {
    const open = (action: () => void) => () => {
      this.host.ui.closePicker()
      action()
    }
    const submit = (content: string) => () => this.submitPaletteCommand(content)
    const prefill = (content: string) => () => {
      this.host.ui.closePicker()
      this.host.ui.composer.value = `${content} `
      this.host.ui.composer.focus()
    }
    const actions: PaletteAction[] = [
      ...(Object.values(this.host.children.presentedState().turns).some((turn) => turn.status === "running")
        ? [{ id: "interrupt.run", title: "Interrupt turn", section: "Conversation", description: "Stop the active turn", run: submit("/interrupt") } satisfies PaletteAction]
        : []),
      { id: "compact.run", title: "Compact context", section: "Conversation", description: "Compact the conversation context", run: submit("/compact") },
      { id: "rewind.run", title: "Rewind to a turn", section: "Conversation", description: "Choose from completed user turns", run: open(() => this.host.ui.openTimelinePicker()) },
      { id: "fork.run", title: "Fork session", section: "Conversation", description: "Fork at the latest completed turn", run: open(() => void this.host.requestFork(null)) },
      { id: "session.new", title: "New session", section: "Conversation", description: this.paletteDescription("Start a clean conversation", "new_session"), run: open(() => void this.host.sessions.createSession()) },
      { id: "session.list", title: "Switch session", section: "Conversation", description: this.paletteDescription("Resume another durable session", "open_session_picker"), run: open(() => this.host.ui.openSessionPicker()) },
      { id: "review.open", title: "Review changes", section: "Conversation", description: this.paletteDescription("Open the cumulative session diff", "open_review"), run: open(() => this.host.ui.openReview()) },
      { id: "session.export", title: "Export session", section: "Conversation", description: "Save this session's transcript to a file", run: open(() => this.host.ui.openExportSessionPicker()) },
      { id: "plan.show", title: "Show plan", section: "Conversation", description: "Display the pending or approved plan", run: submit("/plan") },
      { id: "queue.manage", title: "Manage queued messages", section: "Conversation", description: "Review, remove, or clear queued messages", run: open(() => this.host.ui.openQueuedMessagesPicker()) },
      { id: "cost.show", title: "Show usage & cost", section: "Conversation", description: "Display tokens, cost, and budget", run: submit("/cost") },

      { id: "model.list", title: "Switch model", section: "Agents & models", description: this.paletteDescription("Choose the active model alias", "open_model_picker"), run: open(() => this.host.ui.openModelPicker()) },
      { id: "provider.list", title: "Switch provider route", section: "Agents & models", description: "Choose a configured provider and model route", run: open(() => this.host.ui.openProviderPicker()) },
      { id: "mode.list", title: "Switch mode", section: "Agents & models", description: this.paletteDescription("Choose discuss, plan, or execute", "open_mode_picker"), run: open(() => this.openModePicker()) },
      { id: "agent.children", title: "Child agents", section: "Agents & models", description: this.paletteDescription("Inspect, resume, interrupt, or close child agents", "open_subagent_picker"), run: open(() => this.host.ui.openSubagentPicker()) },
      ...(this.host.children.activeId === null ? [] : [{
        id: "agent.current.actions",
        title: "Current child actions",
        section: "Agents & models",
        description: "Inspect, continue, interrupt, or close the visible child",
        run: open(() => this.host.ui.openSubagentActionPicker(this.host.children.activeId)),
      } satisfies PaletteAction]),
      { id: "status.show", title: "Show agent status", section: "Agents & models", description: "Display running and queue state", run: submit("/status") },

      { id: "view.conversation", title: "View conversation", section: "Workspace", description: "Return to the conversation transcript", run: open(() => this.host.ui.showConversationView()) },
      { id: "view.tools", title: "View tools", section: "Workspace", description: "Inspect retained tool activity and output", run: open(() => this.host.ui.showToolsView()) },
      { id: "workspace.add", title: "Add workspace directory", section: "Workspace", description: "Prefills /add-dir · give a directory path", run: prefill("/add-dir") },
      { id: "workspace.roots", title: "Workspace roots", section: "Workspace", description: "See every live workspace root", run: open(() => this.openWorkspaceRootsPicker()) },
      { id: "trust.manage", title: "Folder trust", section: "Workspace", description: "Show, grant, or revoke folder trust", run: open(() => this.host.ui.openTrustPicker()) },
      { id: "context.manage", title: "Manage context", section: "Workspace", description: "Inspect, pin, or evict context items", run: submit("/context") },

      { id: "permissions.mode", title: "Permission mode", section: "Safety", description: "Choose when tool use needs confirmation", run: open(() => this.host.ui.openPermissionModePicker()) },
      { id: "permissions.manage", title: "Permission rules", section: "Safety", description: "Inspect, add, and remove session rules", run: open(() => this.host.ui.openPermissionPicker()) },
      { id: "budget.manage", title: "Budget limits", section: "Safety", description: "Set spend and subscription-token limits", run: open(() => this.host.ui.openBudgetPicker()) },

      { id: "theme.list", title: "Switch theme", section: "Appearance & settings", description: "Preview and choose an interface theme", run: open(() => this.host.ui.openThemePicker()) },
      { id: "settings.open", title: "Settings", section: "Appearance & settings", description: "Change safe persisted user settings", run: open(() => this.host.ui.openSettingsPicker()) },
      { id: "mcp.manage", title: "MCP connections", section: "Appearance & settings", description: "Add, review, enable, disable, or remove MCP servers", run: open(() => this.host.ui.openMcpPicker()) },

      { id: "keyboard.help", title: "Keyboard shortcuts", section: "Help & system", description: "Every binding for the active preset", run: open(() => this.openKeyboardHelpPicker()) },
      { id: "help.show", title: "Command help", section: "Help & system", description: "List every available slash command", run: submit("/help") },
      { id: "app.exit", title: "Exit Rottweiler", section: "Help & system", description: "Close the TUI and its supervised engine", run: open(() => this.host.onExit?.()) },
    ]
    for (const command of this.host.ui.state.commands) {
      if (isTuiHandledSlashCommand(command.name)) continue
      const requiresArgument = /<[^>]+>/.test(command.usage)
      actions.push({
        id: `slash.${command.name}`,
        title: `/${command.name}`,
        section: "Commands",
        description: `${commandSourceLabel(command.source)} · ${command.description}`,
        catalogSource: command.source === undefined || command.source === "builtin"
          ? "builtin"
          : "extension",
        sourceLabel: commandSourceLabel(command.source).toLocaleLowerCase(),
        detailDescription: command.description,
        run: requiresArgument ? prefill(`/${command.name}`) : submit(`/${command.name}`),
      })
    }
    return actions
  }

  updateComposerAutocomplete(value: string): void {
    const slash = /^\/([^\s]*)$/.exec(value)
    if (slash !== null) {
      this.host.pickerController.anchored = true
      this.host.pickerController.query = slash[1] ?? ""
      this.host.pickerController.position(true)
      this.host.pickerController.kind = "commands"
      if (this.host.ui.state.commands.length === 0 && !this.#commandsRequested) {
        this.requestCommands()
      }
      this.host.pickerController.refresh()
      return
    }
    const mention = /(?:^|\s)@([^\n]*)$/.exec(value)
    if (mention === null && this.host.pickerController.anchored) this.host.ui.closePicker()
  }

  requestCommands(): void {
    this.#commandsRequested = true
    this.host.clearProjectionError("commands")
    this.host.requests.command({ type: "list_commands" })
  }

  requestModes(): void {
    this.host.clearProjectionError("modes")
    this.host.requests.command({ type: "list_modes" })
  }

  openCommandPicker(): void {
    if (this.host.ui.picker.visible) this.host.ui.picker.close()
    this.host.pickerController.begin("palette")
    this.host.ui.commandPalette.resizeForTerminal(
      this.host.terminalWidth,
      this.host.terminalHeight,
    )
    if (!this.#commandsRequested) {
      this.requestCommands()
    }
    this.host.pickerController.refresh()
  }

  openKeyboardHelpPicker(): void {
    this.host.pickerController.begin("keyboardHelp")
    this.host.pickerController.refresh()
  }

  openFilePicker(query = "", anchored = false): void {
    this.host.pickerController.begin("files", anchored, query)
    this.host.requests.command({ type: "search_workspace_files", query, limit: 100 })
    this.host.pickerController.refresh()
    if (!anchored) this.host.ui.picker.input.value = query
  }

  openAttachmentPicker(): void {
    this.host.pickerController.begin("attachments")
    this.host.pickerController.refresh()
  }

  openWorkspaceRootsPicker(): void {
    this.host.pickerController.begin("workspaceRoots")
    this.host.pickerController.refresh()
  }
  openModePicker(): void {
    this.host.pickerController.begin("modes")
    this.requestModes()
    this.host.pickerController.refresh()
  }

}
