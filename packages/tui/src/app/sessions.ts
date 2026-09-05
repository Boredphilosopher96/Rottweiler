import { homedir } from "node:os"
import type { ComposerRenderable, FuzzyPickerRenderable, StateBannerRenderable, PickerItem } from "../components"
import type { PickerController } from "../picker-controller"
import type { ProjectionRequestBroker, ProjectionKind } from "../projection-requests"
import type { Attachment, CommandOutcome, EngineEvent } from "../protocol"
import { presentError } from "../render"
import { isU64 } from "../session-commands"
import type { RottweilerState } from "../state"
import type { RottweilerTheme } from "../theme"
import { isRecord } from "../transport"
import { boundedUiText, queuedMessageLabel, timelineTurnLabel } from "../ui-presentation"

type SessionPickerKind = "timeline" | "timelineActions" | "queuedMessages" | "exportFormat" | "exportOverwrite" | "exportPath" | "sessions" | "sessionActions" | "sessionRename"
interface SessionUiHost {
  readonly state: RottweilerState
  readonly sessionId: string
  readonly picker: FuzzyPickerRenderable<unknown>
  readonly composer: ComposerRenderable
  readonly banner: StateBannerRenderable
  readonly theme: RottweilerTheme
  readonly pickerController: PickerController
  readonly requests: ProjectionRequestBroker
  readonly projectionErrors: Partial<Record<ProjectionKind, string>>
  readonly destroyed: boolean
  composerNotice: string | null
  refresh(): void
  closePicker(): void
  selectSession(sessionId: string): void | Promise<void>
  sendMessage(content: string, attachments: readonly Attachment[], preserveRewindIntent?: boolean): Promise<boolean>
  projectError(code: string, message: string, retryable?: boolean): void
  projectRejection(outcome: Extract<CommandOutcome, { type: "rejected" }>): void
}
function safeErrorMessage(error: unknown): string {
  return error instanceof Error && error.message.length > 0 ? error.message : "the request could not be delivered to the engine"
}
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


type QueuedMessagePickerAction =
  | { readonly kind: "remove"; readonly position: string }
  | { readonly kind: "clear" }

type SessionProjection = RottweilerState["sessions"][number]

type SessionListAction =
  | { readonly kind: "new" }
  | { readonly kind: "session"; readonly session: SessionProjection }
  | { readonly kind: "retry" }

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


function expandLeadingHome(path: string): string {
  if (path === "~") return homedir()
  return path.startsWith("~/") ? `${homedir()}${path.slice(1)}` : path
}


export class SessionUiController {
  readonly #host: SessionUiHost
  #pendingSessionCreateRequestId: string | null = null
  #sessionSearchTimer: ReturnType<typeof setTimeout> | null = null
  #exportNoticeTimer: ReturnType<typeof setTimeout> | null = null
  #sessionActionId: string | null = null
  #timelineTurn: TimelineTurnChoice | null = null
  #pendingRewindIntent: PendingRewindIntent | null = null
  #pendingExport: PendingExport | null = null
  constructor(host: SessionUiHost) { this.#host = host }
  get pending(): boolean { return this.#pendingRewindIntent !== null || this.#pendingExport !== null || this.#pendingSessionCreateRequestId !== null }
  clearRewind(): boolean { const pending = this.#pendingRewindIntent !== null; this.#pendingRewindIntent = null; return pending }
  bindRewindRequest(requestId: string): void { if (this.#pendingRewindIntent !== null) this.#pendingRewindIntent.requestId = requestId }
  pickerClosed(): void { this.#sessionActionId = null; this.clearSessionSearchTimer() }
  reset(): void { this.#timelineTurn = null; this.#pendingRewindIntent = null; this.#pendingExport = null; this.#pendingSessionCreateRequestId = null; this.pickerClosed(); this.clearExportNotice() }
  afterEvent(event: EngineEvent, eventRecord: Record<string, unknown>, commandRequestId: string | null, next: RottweilerState): void {
    const pendingRewind = this.#pendingRewindIntent
    const causedBy = isRecord(eventRecord.meta) && typeof eventRecord.meta.caused_by === "string"
      ? eventRecord.meta.caused_by
      : null
    const rewindAckOutcome = commandRequestId === null
      ? null
      : next.commandAcks[commandRequestId]?.outcome ?? null
    if (
      event.type === "command_acknowledged" &&
      commandRequestId !== null &&
      commandRequestId === this.#pendingSessionCreateRequestId
    ) {
      const acknowledgement = event
      this.#pendingSessionCreateRequestId = null
      if (acknowledgement.outcome.type === "rejected") {
        this.#host.projectRejection(acknowledgement.outcome)
      } else if (
        acknowledgement.session_id === null ||
        acknowledgement.session_id === undefined
      ) {
        this.#host.projectError(
          "session_create_missing_identity",
          "the engine accepted the new session without returning its identity",
          true,
        )
      } else {
        this.#host.closePicker()
        void this.#host.selectSession(acknowledgement.session_id)
      }
    }
    if (
      pendingRewind !== null &&
      event.type === "command_acknowledged" &&
      commandRequestId === pendingRewind.requestId &&
      rewindAckOutcome?.type === "rejected"
    ) {
      this.#pendingRewindIntent = null
      this.#host.projectRejection(rewindAckOutcome)
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
        this.#host.composer.value = pendingRewind.content
        this.#host.composerNotice = pendingRewind.hadAttachments
          ? "attachments from the original message are not restored"
          : null
        this.#host.composer.focus()
        this.#host.refresh()
      } else if (pendingRewind.action === "retry") {
        void this.#host.sendMessage(pendingRewind.content, [])
      }
    }
    const pendingExport = this.#pendingExport
    if (
      event.type === "command_acknowledged" &&
      pendingExport !== null &&
      pendingExport.requestId !== null &&
      commandRequestId === pendingExport.requestId &&
      event.outcome.type === "rejected"
    ) {
      this.#handleExportRejection(
        event.outcome,
        pendingExport,
      )
    } else if (
      event.type === "session_exported" &&
      event.session_id === this.#host.sessionId &&
      pendingExport !== null &&
      pendingExport.requestId !== null &&
      commandRequestId === pendingExport.requestId
    ) {
      this.#pendingExport = null
      this.#showExportNotice(
        event.output_path,
      )
    }
  }
  openTimelinePicker(): void {
    this.#timelineTurn = null
    this.#host.pickerController.begin("timeline")
    this.#host.pickerController.refresh()
  }

  openQueuedMessagesPicker(): void {
    if (this.#host.state.replay.active) return
    this.#host.pickerController.begin("queuedMessages")
    this.#host.pickerController.refresh()
  }

  openExportSessionPicker(): void {
    if (this.#host.state.replay.active) return
    this.#host.pickerController.begin("exportFormat")
    this.#host.pickerController.refresh()
  }

  #openExportPathPrompt(format: ExportFormat): void {
    const choice = EXPORT_FORMAT_CHOICES.find((item) => item.format === format)
    if (choice === undefined) return
    this.#host.pickerController.kind = "exportPath"
    this.#host.picker.openTextPrompt({ title: "Save to path, e.g. ~/transcript.md", placeholder: `~/rottweiler-export.${choice.extension}`, onSubmit: (value) => {
        this.#host.closePicker()
        const outputPath = expandLeadingHome(value.trim())
        void this.#submitSessionExport(format, outputPath, false)
      }, maxBytes: 4_096, empty: "reject" })
  }

  async #submitSessionExport(
    format: ExportFormat,
    outputPath: string,
    force: boolean,
  ): Promise<void> {
    if (this.#host.state.replay.active) return
    const meta = this.#host.requests.meta()
    const pending: PendingExport = {
      format,
      outputPath,
      force,
      requestId: meta.request_id,
    }
    this.#pendingExport = pending
    try {
      const outcome = await this.#host.requests.emit({
        type: "export_session",
        meta,
        session_id: this.#host.sessionId,
        format,
        output_path: outputPath,
        force,
      })
      if (outcome?.type === "rejected" && this.#pendingExport?.requestId === meta.request_id) {
        this.#handleExportRejection(outcome, pending)
      } else if (outcome === null && this.#pendingExport?.requestId === meta.request_id) {
        this.#pendingExport = null
        this.#host.projectError(
          "session_export_unavailable",
          "the session export was not acknowledged by the engine",
          true,
        )
      }
    } catch (error) {
      if (this.#pendingExport?.requestId !== meta.request_id) return
      this.#pendingExport = null
      this.#host.projectError(
        "session_export_failed",
        `session export failed: ${safeErrorMessage(error)}`,
        true,
      )
    }
  }

  #handleExportRejection(outcome: Extract<CommandOutcome, { type: "rejected" }>, pending: PendingExport): void {
    this.#host.projectRejection(outcome)
    if (!pending.force && outcome.error.message.includes("export output already exists")) {
      this.#pendingExport = { ...pending, requestId: null }
      this.#host.pickerController.begin("exportOverwrite")
      this.#host.pickerController.refresh()
      return
    }
    this.#pendingExport = null
  }

  openSessionPicker(): void {
    this.#sessionActionId = null
    this.#host.pickerController.begin("sessions")
    this.#host.requests.command({ type: "list_sessions" })
    this.#host.pickerController.refresh()
  }

  async createSession(): Promise<void> {
    if (this.#host.state.replay.active) {
      this.#host.projectError(
        "new_session_unavailable_in_replay",
        "return to the live session before starting a new conversation",
      )
      return
    }
    if (this.#pendingSessionCreateRequestId !== null) return
    const cwd = this.#host.state.workspaceRoots?.roots[0]
    if (cwd === undefined) {
      this.#host.projectError(
        "new_session_workspace_unavailable",
        "the engine has not published the current workspace yet; try again after it connects",
        true,
      )
      return
    }
    const meta = this.#host.requests.meta()
    this.#pendingSessionCreateRequestId = meta.request_id
    try {
      const outcome = await this.#host.requests.emit({
        type: "create_session",
        meta,
        cwd,
        model: null,
      })
      if (this.#pendingSessionCreateRequestId !== meta.request_id) return
      if (outcome?.type === "rejected") {
        this.#pendingSessionCreateRequestId = null
        this.#host.projectRejection(outcome)
      } else if (outcome === null) {
        this.#pendingSessionCreateRequestId = null
        this.#host.projectError(
          "new_session_unavailable",
          "the engine connection is unavailable",
          true,
        )
      }
    } catch (error) {
      if (this.#pendingSessionCreateRequestId !== meta.request_id) return
      this.#pendingSessionCreateRequestId = null
      this.#host.projectError(
        "new_session_failed",
        presentError({
          category: "protocol",
          code: "new_session_failed",
          message: safeErrorMessage(error),
          requestId: meta.request_id,
        }).text,
        true,
      )
    }
  }

  #openSessionActionPicker(session: SessionProjection): void {
    this.#sessionActionId = session.sessionId
    this.#host.pickerController.kind = "sessionActions"
    this.#host.pickerController.refresh()
  }

  #openSessionRenamePrompt(session: SessionProjection): void {
    this.#sessionActionId = session.sessionId
    this.#host.pickerController.kind = "sessionRename"
    this.#host.picker.openTextPrompt({ title: "Rename session, e.g. Auth refactor", placeholder: session.title ?? session.workspaceName, onSubmit: (title) => {
        const sessionId = this.#sessionActionId
        this.#host.pickerController.kind = "sessions"
        this.#host.pickerController.query = ""
        if (sessionId !== null) {
          this.#host.requests.command({ type: "rename_session", sessionId, title })
        }
        this.#host.pickerController.refresh()
      }, maxBytes: 288, empty: "reject" })
  }

  #showExportNotice(path: string): void {
    this.clearExportNotice()
    this.#host.banner.visible = true
    this.#host.banner.fg = this.#host.theme.success
    this.#host.banner.content = `Exported to ${path}`
    this.#exportNoticeTimer = setTimeout(() => {
      this.#exportNoticeTimer = null
      if (!this.#host.destroyed) this.#host.refresh()
    }, 3_000)
  }

  clearExportNotice(): void {
    if (this.#exportNoticeTimer === null) return
    clearTimeout(this.#exportNoticeTimer)
    this.#exportNoticeTimer = null
  }

  #timelineTurns(): readonly TimelineTurnChoice[] {
    return this.#host.state.transcript
      .filter(
        (entry) =>
          entry.turn.role === "user" &&
          this.#host.state.turns[entry.agentTurn]?.status !== "running" &&
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
    const tools = Object.values(this.#host.state.tools).filter((tool) => tool.turnId === agentTurn)
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
      const accepted = await this.#host.sendMessage(`/rewind ${target}`, [], true)
      if (!accepted && this.#pendingRewindIntent === intent) this.#pendingRewindIntent = null
    } catch (error) {
      if (this.#pendingRewindIntent !== intent) return
      this.#pendingRewindIntent = null
      this.#host.projectError(
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

  scheduleSessionSearch(query: string): void {
    this.clearSessionSearchTimer()
    this.#sessionSearchTimer = setTimeout(() => {
      this.#sessionSearchTimer = null
      if (this.#host.pickerController.kind === "sessions" && this.#host.picker.input.value === query) {
        if (query.trim().length === 0) {
          this.#host.requests.command({ type: "list_sessions" })
        } else {
          this.#host.requests.command({ type: "search_sessions", query, limit: 100 })
        }
      }
    }, 80)
  }

  clearSessionSearchTimer(): void {
    if (this.#sessionSearchTimer !== null) {
      clearTimeout(this.#sessionSearchTimer)
      this.#sessionSearchTimer = null
    }
  }

  render(kind: SessionPickerKind): void {
    switch (kind) {
      case "timeline": {
        const turns = this.#timelineTurns()
        if (turns.length === 0) {
          this.#host.pickerController.showStatus(
            "Conversation timeline",
            "No completed user turns",
            this.#host.state.replay.active ? "read-only session" : "Send a message to create a checkpoint.",
          )
          break
        }
        const readOnly = this.#host.state.replay.active
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
        this.#host.pickerController.show("Conversation timeline", items, (item) => {
          if (item.value === null || readOnly) return
          this.#timelineTurn = item.value
          this.#host.pickerController.kind = "timelineActions"
          this.#host.pickerController.refresh()
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
        this.#host.pickerController.show(`Turn ${turn.agentTurn} actions`, items, (item) => {
          this.#host.closePicker()
          void this.#startRewindIntent(turn, item.value)
        })
        break
      }
      case "queuedMessages": {
        if (this.#host.state.replay.active) {
          this.#host.closePicker()
          break
        }
        const queuedMessages = this.#host.state.queuedMessages
        if (queuedMessages.length === 0) {
          this.#host.pickerController.showStatus(
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
        this.#host.pickerController.show("Queued messages · select to remove", items, (item) => {
          if (item.value.kind === "clear") {
            this.#host.closePicker()
            this.#host.requests.command({ type: "clear_queued_messages" })
            return
          }
          this.#host.requests.command({
            type: "remove_queued_message",
            position: item.value.position,
          })
        })
        break
      }
      case "exportFormat": {
        if (this.#host.state.replay.active) {
          this.#host.closePicker()
          break
        }
        this.#host.pickerController.show(
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
        if (this.#host.state.replay.active || pending === null) {
          this.#host.closePicker()
          break
        }
        this.#host.pickerController.show(
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
            this.#host.closePicker()
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
      case "sessions":
        const sessionError = this.#host.projectionErrors.sessions
        if (
          sessionError === undefined &&
          this.#host.requests.current("sessions") !== null &&
          this.#host.state.sessions.length === 0
        ) {
          this.#host.pickerController.showLoading("Sessions", "Loading sessions")
          break
        }
        const sessionItems: PickerItem<SessionListAction>[] = [
          {
            id: "sessions.new",
            label: "New session",
            description: "Start a clean conversation in this workspace",
            value: { kind: "new" },
          },
          ...(sessionError === undefined
            ? []
            : [{
                id: "sessions.error",
                label: "Couldn't load sessions",
                description: `${sessionError} · select to retry`,
                value: { kind: "retry" } as const,
              }]),
          ...this.#host.state.sessions.map((session) => ({
            id: session.sessionId,
            label: session.title || session.workspaceName,
            description: `${session.workspaceName} · ${session.model}${session.shellActive ? " · shell active" : ""}`,
            searchText: `${session.sessionId} ${session.title ?? ""} ${session.workspaceName} ${session.model}`,
            value: { kind: "session", session } as const,
          })),
        ]
        this.#host.pickerController.show(
          this.#host.state.sessionSearch?.truncated === true
            ? "Sessions · results truncated"
            : "Sessions",
          sessionItems,
          (item) => {
            if (item.value.kind === "new") {
              void this.createSession()
              return
            }
            if (item.value.kind === "retry") {
              const query = this.#host.picker.input.value.trim()
              if (query.length === 0) {
                this.#host.requests.command({ type: "list_sessions" })
              } else {
                this.#host.requests.command({ type: "search_sessions", query, limit: 100 })
              }
              return
            }
            this.#openSessionActionPicker(item.value.session)
          },
        )
        break
      case "sessionActions": {
        const session = this.#host.state.sessions.find(
          (candidate) => candidate.sessionId === this.#sessionActionId,
        )
        if (session === undefined) {
          this.#host.closePicker()
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
        this.#host.pickerController.show(
          `Session actions · ${boundedUiText(session.title ?? session.workspaceName, 64)}`,
          items,
          (item) => {
            if (item.value.kind === "resume") {
              this.#host.closePicker()
              void this.#host.selectSession(item.value.session.sessionId)
            } else {
              this.#openSessionRenamePrompt(item.value.session)
            }
          },
        )
        break
      }
      case "sessionRename":
        break
    }
  }
}
