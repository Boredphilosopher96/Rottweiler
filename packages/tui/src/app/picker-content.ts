import { ErrorUiController } from "./errors"
import { ContextUiController } from "./context"
import type { UiContributionController } from "./ui-contributions"
import type { RottweilerApp } from "../app"
import {
  commandQuery,
  createCommandList,
  createCommandPaletteModel,
  rememberCommand,
  type CommandPaletteCatalog,
} from "../command-palette"
import type { ListDetailPresentation, PickerItem } from "../components"
import { SlashPopupRenderable } from "../components/slash-popup"
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
import { catalogCommand, type CatalogScreenName } from "../session-commands"
import type { RottweilerState } from "../state"
import type { RottweilerTheme } from "../theme"
import type { RenderContext } from "@opentui/core"
import { modePickerPresentation } from "../ui-presentation"
import type { InputUiController } from "./input"
import type { ChildUiController } from "./children"
import type { AgentsScreenController } from "./agents-screen"
import type { SkillsScreenController } from "./skills-screen"
import type { SessionUiController } from "./sessions"
import type { ProviderUiController } from "./provider"
import type { PermissionUiController } from "./permissions"
import type { SettingsUiController } from "./settings"
import type { McpUiController } from "./mcp"
import type { ThemeUiController } from "./themes"
import { commandDetail, commandEntries, type PaletteAction } from "./command-catalog"
export type { PaletteAction } from "./command-catalog"
interface PickerContentHost {
  readonly ui: Pick<RottweilerApp,
    | "picker"
    | "closePicker"
    | "commandPalette"
    | "composer"
    | "openBudgetPicker"
    | "openContextPicker"
    | "openCostPicker"
    | "openMcpPicker"
    | "openModelPicker"
    | "openPermissionPicker"
    | "openQueuedMessagesPicker"
    | "openReview"
    | "openSessionPicker"
    | "openSettingsPicker"
    | "openSubagentPicker"
    | "openThemePicker"
    | "openTimelinePicker"
    | "state"
    | "statusLine"
    | "setState"
  >
  readonly pickerController: PickerController
  readonly input: InputUiController
  readonly requests: ProjectionRequestBroker
  readonly projectionErrors: Partial<Record<ProjectionKind, string>>
  readonly terminalWidth: number
  readonly terminalHeight: number
  readonly sessionId: string
  readonly theme: RottweilerTheme
  readonly children: ChildUiController
  readonly agents: AgentsScreenController
  readonly skills: SkillsScreenController
  readonly sessions: SessionUiController
  readonly providers: ProviderUiController
  readonly permissions: PermissionUiController
  readonly settings: SettingsUiController
  readonly mcp: McpUiController
  readonly themes: ThemeUiController
  readonly contributions: UiContributionController
  onExit(): void
  modalOpened(): void
  clearProjectionError(kind: ProjectionKind): void
  sendMessage(content: string, attachments: readonly Attachment[]): Promise<boolean>
}
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
  #recent: readonly string[] = []
  #slashPopup: SlashPopupRenderable<PaletteAction["action"]> | null = null
  #slashDismissed: string | null = null
  readonly context: ContextUiController
  readonly errors: ErrorUiController
  constructor(readonly host: PickerContentHost) { this.context = new ContextUiController(host); this.errors = new ErrorUiController(host) }
  get commandsRequested(): boolean { return this.#commandsRequested }
  get slashPopup(): SlashPopupRenderable<PaletteAction["action"]> | null { return this.#slashPopup }
  /** Most recently run command ids, newest first. */
  get recentCommands(): readonly string[] { return this.#recent }
  resetCommands(): void { this.#commandsRequested = false }

  /** Build the composer-anchored slash popup for one themed surface. */
  mountSlashPopup(context: RenderContext, theme: RottweilerTheme): SlashPopupRenderable<PaletteAction["action"]> {
    this.#slashPopup = new SlashPopupRenderable<PaletteAction["action"]>(context, theme, {
      onComplete: (entry) => this.#completeSlash(entry as PaletteAction),
      onRun: (entry) => this.#runSlash(entry as PaletteAction),
      onDismiss: () => {
        this.#slashDismissed = this.host.ui.composer.value
        this.#slashPopup?.hide()
      },
      onClear: () => {
        this.#slashDismissed = null
        this.host.ui.composer.value = ""
      },
      active: () => this.host.ui.composer.editor.focused,
      anchorRow: () => this.host.terminalHeight - this.host.ui.composer.dockHeight - this.host.ui.statusLine.height,
    })
    return this.#slashPopup
  }

  /** Every discoverable command for the current session state. */
  commandEntries(): readonly PaletteAction[] {
    const state = this.host.ui.state
    return commandEntries({
      state,
      bindings: this.host.input.bindings,
      childAgents: state.subagentOrder.length > 0 || this.host.children.activeId !== null,
      availability: this.host.projectionErrors.commands !== undefined
        ? "failed"
        : this.#commandsRequested ? "checking" : "ready",
      catalogError: this.host.projectionErrors.commands ?? null,
    })
  }

  renderPicker(): void {
    if (this.host.pickerController.kind !== null) this.hideSlashPopup()
    switch (this.host.pickerController.kind) {
      case "errors": this.errors.render(); break;
      case "context": case "contextItems": case "cost": this.context.render(this.host.pickerController.kind); break
      case "uiPanels": this.host.contributions.renderPicker(); break;
      case "palette": {
        const state = this.host.ui.state
        const catalog: CommandPaletteCatalog = this.host.projectionErrors.commands !== undefined
          ? { kind: "error", message: this.host.projectionErrors.commands }
          : this.#commandsRequested && state.commands.length === 0
            ? { kind: "loading" }
            : { kind: "ready", truncated: state.commandsTruncated }
        const query = this.host.ui.commandPalette.visible
          ? this.host.ui.commandPalette.input.value
          : this.host.pickerController.query
        const preserveSelection = query === this.host.pickerController.query
        this.host.pickerController.query = query
        const model = createCommandPaletteModel({
          entries: this.commandEntries(),
          query,
          recent: this.#recent,
          selectedId: this.host.ui.commandPalette.visible && preserveSelection
            ? this.host.ui.commandPalette.selectedId
            : null,
          catalog,
        })
        const close = this.paletteBinding("open_command_picker")
        const presentation: ListDetailPresentation<PaletteAction> = {
          title: "COMMANDS",
          query,
          selectedId: model.selectedId,
          rows: model.rows.map((row) => row.kind === "section"
            ? row
            : {
                kind: "item",
                id: row.id,
                label: row.entry.title,
                disabled: row.entry.unavailableReason !== null,
                matchSpans: row.titleMatches,
                detail: {
                  title: row.entry.title,
                  description: commandDetail(row.entry, state),
                  meta: [row.entry.section, row.entry.sourceLabel, row.entry.keycap]
                    .filter((part): part is string => part !== null).join(" · "),
                },
                action: row.entry,
              }),
          status: `${model.status} · Enter run${close === null ? " · Esc close" : ` · ${close} close`}`,
          emptyCopy: model.total === 0 ? "No commands available" : "No matching commands",
          notice: model.notice === null
            ? null
            : {
                message: model.notice.message,
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
          this.host.ui.commandPalette.open(presentation, (entry) => this.runCommand(entry), {
            onQuery: () => {
              this.renderPicker()
            },
          })
          this.host.modalOpened()
        }
        break
      }
      case "keyboardHelp": {
        const items: PickerItem<PaletteAction | null>[] = []
        let section: string | null = null
        for (const entry of this.commandEntries()) {
          if (entry.action.kind === "retry") continue
          if (entry.section !== section) {
            section = entry.section
            items.push({ id: `help.section.${section}`, label: section, description: "", value: null, sectionHeader: true })
          }
          const usage = `/${entry.name}${entry.argumentHint.length === 0 ? "" : ` ${entry.argumentHint}`}`
          items.push({
            id: `help.${entry.id}`,
            label: usage,
            ...(entry.keycap === null ? {} : { hint: entry.keycap }),
            description: entry.description,
            detail: [entry.description, "", ...[entry.keycap, entry.sourceLabel].filter((part): part is string => part !== null),
              ...(entry.unavailableReason === null ? [] : ["", entry.unavailableReason])].join("\n"),
            searchText: `${usage} ${entry.title} ${entry.description} ${entry.aliases.join(" ")}`,
            ...(entry.unavailableReason === null ? {} : { tone: "muted" as const, primary: null }),
            value: entry,
          })
        }
        for (const context of KEYBOARD_HELP_CONTEXTS[this.host.input.bindings.preset]) {
          const bindings = this.host.input.bindings.bindings(context)
          if (bindings.size === 0) continue
          items.push({ id: `keyboard-help.section.${context}`, label: `Keys · ${KEYBOARD_HELP_CONTEXT_NAMES[context]}`,
            description: "", value: null, sectionHeader: true })
          for (const [stroke, action] of bindings) {
            const keycap = formatKeycap(stroke)
            const label = KEYBINDING_ACTION_LABELS[action]
            items.push({ id: `keyboard-help.${context}.${stroke}`, label, hint: keycap, description: keycap,
              searchText: `${keycap} ${label}`, primary: null, value: null })
          }
        }
        this.host.pickerController.show("HELP   commands and keys   /help", items, (item) => {
          if (item.value !== null) this.runCommand(item.value)
        }, { primary: "run" })
        break
      }
      case "timeline": this.host.sessions.render("timeline"); break
      case "timelineActions": this.host.sessions.render("timelineActions"); break
      case "queuedMessages": this.host.sessions.render("queuedMessages"); break
      case "exportFormat": this.host.sessions.render("exportFormat"); break
      case "exportOverwrite": this.host.sessions.render("exportOverwrite"); break
      case "exportPath": this.host.sessions.render("exportPath"); break
      case "workspaceRoots": {
        const workspaceRoots = this.host.ui.state.workspaceRoots
        if (workspaceRoots === null) {
          this.host.pickerController.showLoading("WORKSPACE   /dirs", "Loading workspace directories")
          break
        }
        this.host.pickerController.show(
          "WORKSPACE   /dirs",
          workspaceRoots.roots.map((root, index) => ({
            id: `workspace.root.${index}`,
            label: root,
            hint: index === 0 ? "primary" : "added",
            ...(index === 0 ? { marker: "●" } : {}),
            description: index === 0 ? "The session's workspace" : "An additional directory the agent can read and edit",
            detail: `${root}\n\n${index === 0
              ? "The session's primary workspace: tools run here and project instructions are read from here."
              : "An additional directory the agent can read and edit in this session."}\n\nAdd another with /add-dir <path>.`,
            primary: null,
            value: root,
          })),
          () => {},
        )
        break
      }
      case "files": {
        const fileError = this.host.projectionErrors.files
        const anchored = this.host.pickerController.anchored
        const title = anchored ? "@ files" : "FILES   attach to this message"
        if (fileError === undefined && this.host.requests.current("files") !== null && this.host.ui.state.workspaceFiles.length === 0) {
          this.host.pickerController.showLoading(title, "Searching workspace files")
          break
        }
        if (fileError === undefined && this.host.ui.state.workspaceFiles.length === 0) {
          this.host.pickerController.showStatus(title, "No matching files", "Keep typing a path, or try a different name.")
          break
        }
        const fileItems: PickerItem<RottweilerState["workspaceFiles"][number] | null>[] = [
          ...(fileError === undefined ? [] : [{
            id: "files.error", label: "Retry file search", tone: "error" as const, primary: "retry",
            description: fileError, value: null,
          }]),
          ...this.host.ui.state.workspaceFiles.map((file) => ({
            id: file.path,
            label: file.isDirectory ? `${file.path.replace(/\/$/u, "")}/` : file.path,
            hint: file.isDirectory ? "dir" : "file",
            description: file.isDirectory ? "Open this directory" : "Attach this file to the message",
            primary: file.isDirectory ? "open" : "attach",
            value: file,
          })),
        ]
        this.host.pickerController.show(title, fileItems, (item) => {
          const file = item.value
          if (file === null) {
            this.openFilePicker(this.host.pickerController.query, this.host.pickerController.anchored)
            return
          }
          if (file.isDirectory) {
            const query = `${file.path.replace(/\/$/, "")}/`
            if (this.host.pickerController.anchored) {
              const mention = this.host.ui.composer.currentFileMention()
              if (mention !== null) this.host.ui.composer.replaceRange(mention.start, mention.end, `@${query}`)
            } else {
              this.openFilePicker(query)
            }
            return
          }
          const draft = this.host.ui.composer.value
          const mention = this.host.pickerController.anchored ? this.host.ui.composer.currentFileMention() : null
          const requestId = this.host.requests.command({ type: "preview_workspace_file", path: file.path, max_bytes: 5_242_880 })
          if (requestId !== null) {
            this.host.requests.setFilePreview({
              path: file.path, requestId, draft,
              mention: mention === null ? null : { start: mention.start, end: mention.end },
            })
          }
        }, { primary: "attach" })
        break
      }
      case "attachments": {
        const attachments = this.host.ui.composer.attachments
        if (attachments.length === 0) {
          this.host.pickerController.showStatus("ATTACHMENTS", "No attachments in this draft", "Paste an image, or type @ to attach a file.")
          break
        }
        const items: PickerItem<number>[] = attachments.map((attachment, index) => ({
          id: `attachment:${index}`,
          label: attachment.source_path ?? attachment.name,
          hint: attachment.media_type,
          description: `${attachment.media_type} · sent with this message`,
          primary: null,
          value: index,
        }))
        const remove = (index: number) => {
          this.host.ui.composer.removeAttachment(index)
          if (this.host.ui.composer.attachments.length === 0) this.host.ui.closePicker()
          else this.host.pickerController.refresh()
        }
        this.host.pickerController.show("ATTACHMENTS", items, () => {}, {
          keys: [{ stroke: "ctrl+d", label: "remove", available: item => item !== null,
            run: item => { if (item !== null) remove(item.value) } }],
        })
        break
      }
      case "providerSetup":
        break
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
        const loading = this.host.requests.current("modes") !== null
        const presentation = modePickerPresentation(this.host.ui.state, this.host.projectionErrors.modes, loading)
        if (presentation.items.length === 0) {
          if (loading) this.host.pickerController.showLoading(presentation.title, "Loading agent modes")
          else this.host.pickerController.showStatus(presentation.title, "No agent modes are available", "The engine did not publish any modes for this session.")
          break
        }
        this.host.pickerController.show(presentation.title, presentation.items, (item) => {
          if (item.value.kind === "retry") {
            this.requestModes()
            this.host.pickerController.refresh()
            return
          }
          this.host.requests.dispatch({
            type: "switch_mode",
            meta: this.host.requests.meta(),
            session_id: this.host.sessionId,
            mode: item.value.id,
          })
          this.host.ui.closePicker()
        }, { primary: "switch", selectedId: `mode:${this.host.ui.state.mode}` })
        break
      }
      case "agents": this.host.agents.render("agents"); break
      case "agentActions": this.host.agents.render("agentActions"); break
      case "skills": this.host.skills.render(); break
      case "sessions": this.host.sessions.render("sessions"); break
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

  /** Run one catalog or extension command from the palette, help, or slash popup. */
  runCommand(entry: PaletteAction): void {
    const action = entry.action
    if (action.kind === "retry") {
      this.requestCommands()
      this.host.pickerController.refresh()
      return
    }
    if (entry.unavailableReason !== null) return
    this.#recent = rememberCommand(this.#recent, entry.id)
    const prefill = () => {
      this.host.ui.closePicker()
      this.host.ui.composer.value = `/${entry.name} `
      this.host.ui.composer.editor.gotoBufferEnd()
      this.host.ui.composer.focus()
    }
    if (action.kind === "extension") {
      if (action.requiresArgument) prefill()
      else this.submitPaletteCommand(`/${entry.name}`)
      return
    }
    const command = catalogCommand(action.name)
    if (command === undefined) return
    if (command.target === "engine") {
      if (/<[^>]+>/u.test(command.argument_hint)) prefill()
      else this.submitPaletteCommand(`/${command.name}`)
      return
    }
    this.host.ui.closePicker()
    this.openScreen(command.name as CatalogScreenName)
  }

  /** Records a typed slash command as recently used. */
  rememberSlash(content: string): void {
    const name = /^\s*\/(\S+)/u.exec(content)?.[1]
    if (name === undefined) return
    const entry = this.commandEntries().find((candidate) =>
      candidate.name === name || candidate.aliases.includes(name))
    if (entry !== undefined && entry.action.kind !== "retry") this.#recent = rememberCommand(this.#recent, entry.id)
  }

  /** Open the client screen that owns one catalog entry. */
  openScreen(name: CatalogScreenName): void {
    const ui = this.host.ui
    switch (name) {
      case "new": void this.host.sessions.createSession(); return
      case "resume": ui.openSessionPicker(); return
      case "rewind": ui.openTimelinePicker(); return
      case "queue": ui.openQueuedMessagesPicker(); return
      case "model": ui.openModelPicker(); return
      case "mode": this.openModePicker(); return
      case "agents": ui.openSubagentPicker(); return
      case "context": ui.openContextPicker(); return
      case "usage": ui.openCostPicker(); return
      case "review": ui.openReview(); return
      case "dirs": this.openWorkspaceRootsPicker(); return
      case "mcp": ui.openMcpPicker(); return
      case "permissions": ui.openPermissionPicker(); return
      case "settings": ui.openSettingsPicker(); return
      case "skills": this.host.skills.open(); return
      case "theme": ui.openThemePicker(); return
      case "help": this.openKeyboardHelpPicker(); return
      case "errors": this.errors.open(); return
      case "exit": this.host.onExit(); return
      default: return assertNever(name)
    }
  }

  hideSlashPopup(): void {
    this.#slashPopup?.hide()
  }

  /** Composer changes drive slash completion, `?` help, and file mentions. */
  updateComposerAutocomplete(value: string): void {
    const composer = this.host.ui.composer
    if (value === "?" && composer.value === "?" && composer.attachments.length === 0) {
      composer.value = ""
      this.openKeyboardHelpPicker()
      return
    }
    this.#updateSlashPopup(value)
    const mention = /(?:^|\s)@([^\n]*)$/.exec(value)
    if (mention === null && this.host.pickerController.anchored) this.host.ui.closePicker()
  }

  #updateSlashPopup(value: string): void {
    const popup = this.#slashPopup
    if (popup === null) return
    if (value !== this.#slashDismissed) {
      this.#slashDismissed = null
      popup.disarmClear()
    }
    const name = /^\/(\S*)$/u.exec(value)
    const invocation = /^\/(\S+)\s/u.exec(value)
    if (this.#slashDismissed !== null || (name === null && invocation === null) || this.host.ui.picker.visible
      || this.host.ui.commandPalette.visible) {
      popup.hide()
      return
    }
    if (!this.#commandsRequested && this.host.ui.state.commands.length === 0 && this.host.projectionErrors.commands === undefined) {
      this.requestCommands()
    }
    const entries = this.commandEntries().filter((entry) => entry.action.kind !== "retry")
    if (invocation !== null) {
      const typed = invocation[1] ?? ""
      const entry = entries.find((candidate) => candidate.name === typed || candidate.aliases.includes(typed))
      if (entry === undefined) popup.hide()
      else popup.show({ kind: "arguments", entry })
      return
    }
    const list = createCommandList(entries, name?.[1] ?? "", this.#recent)
    if (list.visible === 0) {
      popup.hide()
      return
    }
    popup.show({ kind: "commands", query: name?.[1] ?? "", rows: list.rows, selectedId: list.selectedId })
  }

  #completeSlash(entry: PaletteAction): void {
    const composer = this.host.ui.composer
    composer.value = entry.argumentHint.length === 0 ? `/${entry.name}` : `/${entry.name} `
    composer.editor.gotoBufferEnd()
    this.#updateSlashPopup(composer.value)
  }

  #runSlash(entry: PaletteAction): void {
    const typed = commandQuery(this.host.ui.composer.value)
    const requiresArgument = /<[^>]+>/u.test(entry.argumentHint)
    if (requiresArgument && typed !== entry.name) {
      this.#completeSlash(entry)
      return
    }
    this.hideSlashPopup()
    this.host.ui.composer.value = `/${entry.name}`
    void this.host.ui.composer.submit()
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
    this.hideSlashPopup()
    if (this.host.ui.picker.visible) this.host.ui.picker.close()
    this.host.pickerController.begin("palette")
    this.host.ui.commandPalette.resizeForTerminal(
      this.host.terminalWidth,
      this.host.terminalHeight,
      this.host.terminalHeight - this.host.ui.composer.dockHeight - this.host.ui.statusLine.height,
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

function assertNever(value: never): never {
  throw new Error(`unhandled command screen ${String(value)}`)
}
