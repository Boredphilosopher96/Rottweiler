import { directSessionRead } from "../session-reader"
import { homedir } from "node:os"
import type {
  ComposerRenderable,
  FuzzyPickerRenderable,
  StateBannerRenderable,
  PickerItem,
} from "../components"
import type { PickerController } from "../picker-controller"
import type { ProjectionRequestBroker, ProjectionKind } from "../projection-requests"
import type { Attachment, CommandOutcome, EngineEvent } from "../protocol"
import { presentError } from "../render"
import type { ComposerDraftStore, DraftSubmission } from "../composer-drafts"
import type { ClientCache } from "../history/cache"
import type { HistoryCacheValue } from "../history/controller"
import type { SessionReader } from "../session-reader"
import { TimelineController, readTimelineDraft, type TimelineChoice } from "../history/timeline"
import type { RottweilerState } from "../state"
import type { RottweilerTheme } from "../theme"
import { isRecord } from "../transport"
import { boundedUiText, queuedMessageLabel, timelineTurnLabel } from "../ui-presentation"

type SessionPickerKind = "timeline" | "timelineActions" | "queuedMessages" | "exportFormat" | "exportOverwrite" | "exportPath" | "sessions" | "sessionActions" | "sessionRename"
interface SessionUiHost {
  readonly sessionReader: SessionReader
  readonly historyCache: ClientCache<HistoryCacheValue>
  readonly drafts: ComposerDraftStore
  readonly draftScope: string
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
  navigateTranscript(sequence: string): Promise<import("../protocol").TranscriptAnchor | null>
  selectSession(sessionId: string): void | Promise<void>
  sendMessage(content: string, attachments: readonly Attachment[]): Promise<boolean>
  projectError(code: string, message: string, retryable?: boolean): void
  projectRejection(outcome: Extract<CommandOutcome, { type: "rejected" }>): void
}
function safeErrorMessage(error: unknown): string {
  return error instanceof Error && error.message.length > 0 ? error.message : "the request could not be delivered to the engine"
}
type TimelineAction = "edit" | "retry" | "rewind"
interface PendingRewindIntent {
  readonly action: TimelineAction
  readonly draft: DraftSubmission | null
  readonly scope: string
  readonly hadAttachments: boolean
  readonly requestId: string | null
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
  #timelineTurn: TimelineChoice | null = null
  #pendingRewindIntent: PendingRewindIntent | null = null
  #pendingExport: PendingExport | null = null
  #timeline: TimelineController | null = null
  #rewindRead: AbortController | null = null
  #navigation: object | null = null
  #retrying = false
  constructor(host: SessionUiHost) { this.#host = host }
  get pending(): boolean { return this.#navigation !== null || this.#retrying || this.#rewindRead !== null || this.#pendingRewindIntent !== null || this.#pendingExport !== null || this.#pendingSessionCreateRequestId !== null }
  clearRewind(): boolean {
    const pending = this.#pendingRewindIntent !== null || this.#rewindRead !== null
    this.#rewindRead?.abort(); this.#rewindRead = null
    this.#pendingRewindIntent?.draft?.settle(true)
    this.#pendingRewindIntent = null
    return pending
  }
  pickerClosed(): void {
    this.#sessionActionId = null; this.clearSessionSearchTimer()
    this.#timeline?.dispose(); this.#timeline = null; this.#timelineTurn = null
  }
  reset(): void { this.#navigation = null; this.#timelineTurn = null; this.clearRewind(); this.#pendingExport = null; this.#pendingSessionCreateRequestId = null; this.pickerClosed(); this.clearExportNotice() }
  afterEvent(event: EngineEvent, eventRecord: Record<string, unknown>, commandRequestId: string | null, next: RottweilerState): void {
    if (event.type === "session_navigation_requested") {
      const state = this.#host.state
      if (event.session_id !== this.#host.sessionId || state.replay.active
        || state.driverClientId !== event.meta.client_id) return
      void this.#navigate(event)
      return
    }
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
      this.clearRewind()
      this.#host.projectRejection(rewindAckOutcome)
    } else if (
      pendingRewind !== null && event.type === "error" && causedBy === pendingRewind.requestId
    ) {
      this.clearRewind()
    } else if (
      pendingRewind !== null && event.type === "conversation_rewound"
      && pendingRewind.requestId !== null && causedBy === pendingRewind.requestId
    ) {
      // Matching durable causation consumes the follow-up exactly once.
      this.#pendingRewindIntent = null
      if (pendingRewind.action === "edit") this.#restoreTimelineDraft(pendingRewind)
      else if (pendingRewind.action === "retry") void this.#retryTimelineDraft(pendingRewind)
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
  get timelineRestorable(): boolean {
    const snapshot = this.#timeline?.history.snapshot
    const selected = this.#host.picker.select.getSelectedOption()?.value
    return snapshot !== undefined && !snapshot.loading && snapshot.error === null
      && (snapshot.total === 0n || (typeof selected === "string" && /^timeline\.turn\.[0-9]+$/.test(selected)))
  }
  openTimelinePicker(anchor?: string): void {
    this.#timelineTurn = null
    this.#host.pickerController.begin("timeline")
    this.#timeline?.dispose()
    this.#timeline = new TimelineController(this.#host.sessionReader, this.#host.historyCache, () => {
      if (this.#host.pickerController.kind === "timeline") this.#host.pickerController.refresh()
    })
    void this.#timeline.open(directSessionRead(this.#host.sessionId), anchor)
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
    using replyAllocation = this.#host.requests.allocate()
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
      }, replyAllocation)
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

  async #navigate(event: Extract<EngineEvent, { type: "session_navigation_requested" }>): Promise<void> {
    if (this.#navigation !== null) {
      this.#host.projectError("navigation_pending", "A session navigation is already pending.", true)
      return
    }
    const request = {}
    this.#navigation = request
    try {
      if (event.target.kind === "session") {
        this.#host.closePicker()
        await this.#host.selectSession(event.target.session_id)
      } else {
        const anchor = await this.#host.navigateTranscript(event.target.sequence)
        if (this.#navigation !== request || this.#host.sessionId !== event.session_id) return
        if (anchor?.type === "replaced") {
          this.#host.composerNotice = anchor.replacement === null ? "The requested transcript item is unavailable."
            : `Transcript item ${anchor.requested} is unavailable; showing item ${anchor.replacement}.`
          this.#host.refresh()
        }
      }
    } catch (error) {
      if (this.#navigation === request && !this.#host.destroyed && this.#host.sessionId === event.session_id) {
        this.#host.projectError("session_navigation_failed", safeErrorMessage(error), true)
      }
    } finally { if (this.#navigation === request) this.#navigation = null }
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
    using replyAllocation = this.#host.requests.allocate()
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
      }, replyAllocation)
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

  #timelineTurnDescription(agentTurn: string, readOnly: boolean): string {
    const tools = Object.values(this.#host.state.tools).filter((tool) => tool.turnId === agentTurn)
    const edits = tools.filter((tool) => tool.diff !== null).length
    const detail = [`turn ${agentTurn}`]
    if (tools.length > 0) detail.push(`${tools.length} ${tools.length === 1 ? "tool" : "tools"}`)
    if (edits > 0) detail.push(`${edits} ${edits === 1 ? "edit" : "edits"}`)
    if (readOnly) detail.push("read-only")
    return detail.join(" · ")
  }

  async #startRewindIntent(turn: TimelineChoice, action: TimelineAction): Promise<void> {
    using replyAllocation = this.#host.requests.allocate()
    if (this.#host.state.replay.active || this.#host.draftScope !== "parent" || this.#retrying) return
    this.clearRewind()
    const request = new AbortController()
    this.#rewindRead = request
    const scope = this.#host.draftScope
    let draft: DraftSubmission | null = null
    try {
      if (turn.view.through === null) throw new Error("The selected source has no committed history prefix.")
      if (action !== "rewind") draft = await readTimelineDraft(this.#host.sessionReader, turn, this.#host.drafts, scope, request.signal, this.#host.historyCache)
      request.signal.throwIfAborted()
      if (this.#host.destroyed || this.#host.sessionId !== turn.view.session_id || this.#host.draftScope !== scope) {
        draft?.settle(true); return
      }
      const meta = this.#host.requests.meta()
      const intent: PendingRewindIntent = { action, draft, scope, hadAttachments: turn.hadAttachments, requestId: meta.request_id }
      this.#pendingRewindIntent = intent
      this.#rewindRead = null
      const outcome = await this.#host.requests.emit({ type: "rewind", meta, session_id: turn.view.session_id,
        target: { type: "source", expected_through: turn.view.through, source: turn.sequenceId,
          turn_id: turn.agentTurn, position: action === "rewind" ? "through" : "before" } }, replyAllocation)
      if (this.#pendingRewindIntent !== intent) return
      if (outcome?.type !== "accepted") {
        this.clearRewind()
        if (outcome?.type === "rejected") this.#host.projectRejection(outcome)
        else throw new Error("The engine connection is unavailable.")
      }
    } catch (error) {
      draft?.settle(true)
      if (request.signal.aborted) return
      this.#pendingRewindIntent = null
      this.#host.projectError("rewind_failed", safeErrorMessage(error), true)
    } finally {
      if (this.#rewindRead === request) this.#rewindRead = null
    }
  }

  #restoreTimelineDraft(intent: PendingRewindIntent): void {
    const restored = intent.draft?.settle(false)
    if (restored == null || this.#host.destroyed || this.#host.draftScope !== intent.scope) return
    this.#host.composer.restoreDraft(restored.content, restored.attachments)
    this.#host.composerNotice = intent.hadAttachments ? "attachments from the original message are not restored" : null
    this.#host.composer.focus()
    this.#host.refresh()
  }

  async #retryTimelineDraft(intent: PendingRewindIntent): Promise<void> {
    if (intent.draft === null) return
    this.#retrying = true
    try {
      const accepted = await this.#host.sendMessage(intent.draft.draft.content, [])
      if (accepted) intent.draft.settle(true)
      else this.#restoreTimelineDraft(intent)
    } catch { this.#restoreTimelineDraft(intent) }
    finally { this.#retrying = false }
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
        const timeline = this.#timeline
        const turns = timeline?.choices ?? []
        if (turns.length === 0 && !timeline?.older && !timeline?.newer) {
          this.#host.pickerController.showStatus(
            "Conversation timeline",
            timeline?.history.snapshot.loading ? "Loading conversation history" : timeline?.history.snapshot.error ?? "No user turns",
            this.#host.state.replay.active ? "read-only session" : "Send a message to create a checkpoint.",
          )
          break
        }
        const readOnly = this.#host.state.replay.active
        const items: PickerItem<TimelineChoice | "older" | "newer" | null>[] = [
          ...(readOnly
            ? [{
                id: "timeline.read-only",
                label: "read-only session",
                description: "Timeline actions are unavailable in replay",
                value: null,
                selectable: false,
              }]
            : []),
          ...(timeline?.newer ? [{ id: "timeline.newer", label: "Newer history", description: "Read the next page", value: "newer" as const }] : []),
          ...(timeline?.older ? [{ id: "timeline.older", label: "Older history", description: "Read the previous page", value: "older" as const }] : []),
          ...turns.map((turn) => ({
            id: `timeline.turn.${turn.sequenceId}`,
            label: timelineTurnLabel(turn.preview),
            description: this.#timelineTurnDescription(turn.agentTurn, readOnly),
            value: turn,
            selectable: !readOnly,
          })),
        ]
        this.#host.pickerController.show("Conversation timeline", items, (item) => {
          if (item.value === "older") { void timeline?.previous(); return }
          if (item.value === "newer") { void timeline?.next(); return }
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
