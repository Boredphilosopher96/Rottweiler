import { childProgressSource } from "../child-source"
import { TodoController } from "../todo-controller"
import { emptyTodos } from "../state/todos"
import { directSessionRead, descendantSessionRead, type SessionReader, type SessionReadTarget } from "../session-reader"
import { ComposerDraftStore } from "../composer-drafts"
import { fg, t } from "@opentui/core"
import {
  formatSubagentElapsed,
  type ComposerRenderable,
  type StateBannerRenderable,
  type PickerItem,
} from "../components"
import type { ClientDiagnostics } from "../client-diagnostics"
import type { HistoryPresentation } from "../history/presentation"
import type { KeybindingAction } from "../keybindings"
import type { PickerController } from "../picker-controller"
import type { ProjectionRequestBroker } from "../projection-requests"
import type { CommandOutcome, EngineEvent } from "../protocol"
import { presentError } from "../render"
import { createInitialState, engineEvent, reduceRottweilerState, type RottweilerState } from "../state"
import {
  boundSubagentState,
  childEngineEvent,
  initialSubagentState,
  sanitizeSubagentDescriptor,
  type ComposerDraft,
  type SubagentDescriptor,
} from "../subagent-state"
import type { RottweilerTheme } from "../theme"
import { boundedUiText } from "../ui-presentation"
interface ChildUiHost {
  state: RottweilerState
  readonly sessionId: string
  readonly composer: ComposerRenderable
  readonly banner: StateBannerRenderable
  readonly theme: RottweilerTheme
  readonly history: HistoryPresentation
  readonly diagnostics: ClientDiagnostics | undefined
  readonly pickerController: PickerController
  readonly requests: ProjectionRequestBroker
  readonly sessionReader: SessionReader
  focus(): void
  refresh(): void
  presentEvent(event: EngineEvent): void
  closePicker(): void
  binding(action: KeybindingAction): string | null
  projectError(code: string, message: string, retryable?: boolean): void
  projectRejection(outcome: Extract<CommandOutcome, { type: "rejected" }>): void
}
function safeErrorMessage(error: unknown): string { return error instanceof Error && error.message.length > 0 ? error.message : "the request could not be delivered to the engine" }
type SubagentAction =
  | { readonly kind: "inspect"; readonly subagent: SubagentDescriptor }
  | { readonly kind: "continue"; readonly subagent: SubagentDescriptor }
  | { readonly kind: "running"; readonly subagent: SubagentDescriptor }
  | { readonly kind: "interrupt"; readonly subagent: SubagentDescriptor }
  | { readonly kind: "close"; readonly subagent: SubagentDescriptor }
const MAX_VISIBLE_SUBAGENTS = 256

export class ChildUiController {
  readonly #host: ChildUiHost
  #scope: object = {}
  #subagentListError: string | null = null
  #subagentDescriptors: readonly SubagentDescriptor[] = []
  #activeChildState: RottweilerState | null = null
  #historicalChild: { readonly sessionId: string; readonly task: string; readonly target: SessionReadTarget } | null = null
  readonly draftStore = new ComposerDraftStore()
  #activeSubagentId: string | null = null
  #subagentActionId: string | null = null
  #subagentErrorBaseline: RottweilerState["errors"][number] | undefined
  #parentReadTarget: SessionReadTarget | null = null
  #activeReadTarget: SessionReadTarget | null = null
  readonly #todos: TodoController
  constructor(host: ChildUiHost) {
    this.#host = host
    this.#todos = new TodoController({
      reader: host.sessionReader,
      state: () => this.#activeChildState?.todos ?? emptyTodos(),
      update: todos => {
        if (this.#activeChildState === null) return
        this.#activeChildState = { ...this.#activeChildState, todos }
        this.#host.refresh()
      },
    })
  }
  retryTodos(): void { this.#todos.retry() }
  refreshTodos(): void {
    const session = this.#historicalChild?.sessionId ?? (this.#activeSubagentId === null
      ? undefined : this.subagentDescriptor(this.#activeSubagentId)?.child_session_id)
    if (session !== undefined) this.#todos.open(this.readTarget)
  }
  get readTarget(): SessionReadTarget {
    if (this.#activeReadTarget !== null) return this.#activeReadTarget
    if (this.#parentReadTarget?.sessionId !== this.#host.sessionId) this.#parentReadTarget = directSessionRead(this.#host.sessionId)
    return this.#parentReadTarget
  }
  get activeId(): string | null { return this.#activeSubagentId }
  get historical(): { readonly sessionId: string; readonly task: string } | null { return this.#historicalChild }
  get drafts(): readonly { readonly id: string; readonly draft: ComposerDraft }[] {
    return this.draftStore.entries().filter(entry => entry.scope.startsWith("child:")).map(entry => ({ id: entry.scope.slice(6), draft: entry.draft }))
  }
  restoreDrafts(parent: ComposerDraft, children: readonly { readonly id: string; readonly draft: ComposerDraft }[]): boolean {
    return this.draftStore.replace([{ scope: "parent", draft: parent },
      ...children.map(({ id, draft }) => ({ scope: `child:${id}`, draft }))])
  }
  reset(): void {
    this.#todos.reset()
    this.#scope = {}
    this.#subagentListError = null; this.#subagentDescriptors = []; this.#activeChildState = null
    this.#historicalChild = null; this.#activeReadTarget = null; this.draftStore.clear(); this.#activeSubagentId = null; this.#subagentActionId = null
    this.#subagentErrorBaseline = undefined
  }
  pickerClosed(): void { this.#subagentActionId = null }
  acceptCatalog(values: readonly SubagentDescriptor[]): void {
    this.#subagentListError = null
    this.#subagentDescriptors = values.slice(0, MAX_VISIBLE_SUBAGENTS).map(sanitizeSubagentDescriptor)
      .filter((descriptor): descriptor is SubagentDescriptor => descriptor !== null)
    if (this.#activeSubagentId !== null && this.subagentDescriptor(this.#activeSubagentId) === undefined && this.#historicalChild === null) this.leaveSubagent()
    else this.#host.refresh()
  }
  openHistorical(child: { readonly sessionId: string; readonly subagentId: string; readonly task: string; readonly sourceSequence: string }): void {
    if (this.subagentDescriptor(child.subagentId)?.child_session_id === child.sessionId) {
      void this.enterSubagent(child.subagentId); return
    }
    let target: SessionReadTarget
    try { target = descendantSessionRead(this.readTarget, { session_id: child.sessionId, subagent_id: child.subagentId, source_sequence: child.sourceSequence }) }
    catch { this.#host.projectError("child_history_scope", "Child history exceeds the permitted ancestry path."); return }
    if (!this.saveComposerDraft()) return
    this.#activeReadTarget = target
    this.#historicalChild = { sessionId: child.sessionId, task: boundedUiText(child.task, 512), target }
    this.#activeSubagentId = child.subagentId
    this.#activeChildState = createInitialState()
    this.#todos.open(target)
    this.restoreComposerDraft(child.subagentId)
    this.#host.refresh(); this.#host.focus()
  }
  responseStarted(id: string): void {
    this.#subagentErrorBaseline = this.#host.state.errors.at(-1)
    this.setSubagentActivity(id, "running")
  }
  openSubagentPicker(): void {
    if (this.#host.state.replay.active) {
      this.#host.projectError(
        "subagents_unavailable_in_replay",
        "Child-agent controls are available from the live parent session, not historical replay.",
      )
      return
    }
    this.#host.pickerController.begin("agents")
    this.requestSubagents()
    this.#host.pickerController.refresh()
  }

  openSubagentActionPicker(subagentId = this.#activeSubagentId): void {
    if (subagentId === null || this.subagentDescriptor(subagentId) === undefined) return
    this.#subagentActionId = subagentId
    this.#host.pickerController.begin("agentActions")
    this.#host.pickerController.refresh()
  }

  requestSubagents(): void {
    if (this.#host.state.replay.active) return
    this.#subagentListError = null
    const meta = this.#host.requests.issue("subagents")
    void this.#host.requests.emit({
      type: "list_subagents",
      meta,
      session_id: this.#host.sessionId,
    }).then((outcome) => {
      if (
        outcome?.type === "rejected" &&
        this.#host.requests.matches("subagents", meta.request_id)
      ) {
        this.#host.requests.clear("subagents")
        this.#subagentListError = presentError({
          category: outcome.error.category,
          code: outcome.error.code,
          message: outcome.error.message,
          requestId: meta.request_id,
        }).text
        this.#host.projectRejection(outcome)
        if (this.#host.pickerController.kind === "agents") this.#host.pickerController.refresh()
      } else if (
        outcome == null &&
        this.#host.requests.matches("subagents", meta.request_id)
      ) {
        this.#host.requests.clear("subagents")
        const presentation = presentError({
          category: "protocol",
          code: "subagents_unavailable",
          message: "Couldn't load child agents because the engine connection is unavailable.",
          requestId: meta.request_id,
        })
        this.#subagentListError = presentation.text
        this.#host.projectError(
          "subagents_unavailable",
          presentation.text,
          true,
        )
        if (this.#host.pickerController.kind === "agents") this.#host.pickerController.refresh()
      }
    }).catch((error) => {
      if (!this.#host.requests.matches("subagents", meta.request_id)) return
      this.#host.requests.clear("subagents")
      const presentation = presentError({
        category: "protocol",
        code: "subagents_failed",
        message: safeErrorMessage(error),
        requestId: meta.request_id,
      })
      this.#subagentListError = presentation.text
      this.#host.projectError("subagents_failed", presentation.text, true)
      if (this.#host.pickerController.kind === "agents") this.#host.pickerController.refresh()
    })
  }

  async enterSubagent(subagentId: string): Promise<void> {
    const descriptor = this.subagentDescriptor(subagentId)
    if (descriptor === undefined) return
    if (!this.saveComposerDraft()) return
    this.#activeSubagentId = subagentId
    this.#historicalChild = null
    this.restoreComposerDraft(subagentId)
    this.#subagentErrorBaseline = this.#host.state.errors.at(-1)
    this.#activeChildState = initialSubagentState(this.#host.state, descriptor)
    this.#activeReadTarget = directSessionRead(descriptor.child_session_id)
    this.#todos.open(this.#activeReadTarget)
    this.#host.refresh()
    this.#host.focus()
  }

  leaveSubagent(): void {
    if (this.#activeSubagentId === null) return
    if (!this.saveComposerDraft()) return
    this.#todos.reset()
    this.#activeSubagentId = null
    this.#activeReadTarget = null
    this.#historicalChild = null
    this.#activeChildState = null
    this.restoreComposerDraft(null)
    this.#subagentActionId = null
    this.#subagentErrorBaseline = undefined
    this.#host.refresh()
    this.#host.focus()
  }

  saveComposerDraft(): boolean {
    const accepted = this.draftStore.set(this.composerScope(), {
      content: this.#host.composer.value, attachments: this.#host.composer.attachments,
    })
    if (!accepted) this.#host.projectError("draft_budget_full", "Draft storage is full. Shorten a draft or remove an attachment before switching.")
    return accepted
  }

  composerScope(): string {
    return this.#activeSubagentId === null ? "parent" : `child:${this.#activeSubagentId}`
  }

  restoreComposerDraft(subagentId: string | null): void {
    const draft = this.draftStore.get(subagentId === null ? "parent" : `child:${subagentId}`)
    this.#host.composer.restoreDraft(draft.content, draft.attachments)
  }

  acceptProgress(event: Extract<EngineEvent, { type: "subagent_progress" }>): boolean {
    if (event.parent_session_id !== this.#host.sessionId) return false
    const descriptor = this.subagentDescriptor(event.subagent_id)
    if (descriptor === undefined || descriptor.child_session_id !== event.child_session_id) return false
    const source = childProgressSource(event)
    if (event.event === null && source !== null) {
      this.#host.history.invalidate(event.child_session_id)
      this.invalidateSubagentSource(event.subagent_id, source)
    }
    const childEvent = childEngineEvent(event.event, event.child_session_id)
    if (childEvent !== null) {
      this.#host.history.invalidate(event.child_session_id)
      if (this.#activeSubagentId === event.subagent_id) this.applySubagentEvent(event.subagent_id, childEvent)
    }
    return this.#host.state.subagents[event.subagent_id]?.childSessionId === event.child_session_id
  }

  invalidateSubagentSource(subagentId: string, sequence: string): void {
    if (subagentId !== this.#activeSubagentId) return
    const descriptor = this.subagentDescriptor(subagentId)
    if (descriptor === undefined) return
    const previous = this.#activeChildState?.lastSequence
    if (previous !== null && previous !== undefined && BigInt(sequence) <= BigInt(previous)) return
    this.#activeChildState = { ...initialSubagentState(this.#host.state, descriptor), lastSequence: sequence }
    this.#todos.open(this.readTarget, sequence)
    this.#host.refresh()
  }

  applySubagentEvent(subagentId: string, event: EngineEvent): void {
    const descriptor = this.subagentDescriptor(subagentId)
    if (descriptor === undefined) return
    const previous = this.#activeChildState ?? initialSubagentState(this.#host.state, descriptor)
    const reducedAt = this.#host.diagnostics?.start()
    const next = boundSubagentState(reduceRottweilerState(previous, engineEvent(event)))
    if (reducedAt !== undefined) this.#host.diagnostics?.finish("reducer", reducedAt)
    this.#activeChildState = next
    this.#todos.event(event)
    this.#subagentErrorBaseline = this.#host.state.errors.at(-1)
    if (event.type === "turn_finished") this.setSubagentActivity(subagentId, "idle")
    else if (event.type === "turn_started") this.setSubagentActivity(subagentId, "running")
    this.#host.presentEvent(event)
  }

  subagentDescriptor(subagentId: string): SubagentDescriptor | undefined {
    return this.#subagentDescriptors.find((subagent) => subagent.subagent_id === subagentId)
  }

  isActiveSubagentRunning(): boolean {
    return this.#activeSubagentId !== null &&
      this.subagentDescriptor(this.#activeSubagentId)?.activity === "running"
  }

  setSubagentActivity(subagentId: string, activity: SubagentDescriptor["activity"]): void {
    this.#subagentDescriptors = this.#subagentDescriptors.map((subagent) =>
      subagent.subagent_id === subagentId ? { ...subagent, activity } : subagent,
    )
  }

  presentedState(): RottweilerState {
    if (this.#activeSubagentId === null) return this.#host.state
    if (this.#activeChildState !== null) return this.#activeChildState
    const descriptor = this.subagentDescriptor(this.#activeSubagentId)
    if (descriptor === undefined) return this.#host.state
    return this.#activeChildState ?? initialSubagentState(this.#host.state, descriptor)
  }

  updateSubagentBanner(state: RottweilerState): void {
    if (this.#activeSubagentId === null) return
    if (this.#historicalChild !== null) {
      this.#host.banner.visible = true
      this.#host.banner.content = `Child transcript · ${this.#historicalChild.task} · Esc parent`
      return
    }
    const descriptor = this.subagentDescriptor(this.#activeSubagentId)
    if (descriptor === undefined) return
    const approval = Object.values(state.tools).some((tool) => tool.status === "awaiting_approval")
    const history = this.#host.history?.controller.snapshot
    const replaying = history?.loading ?? false
    const latestError = this.#host.state.errors.at(-1)
    const hasErrorContext = latestError !== undefined && latestError !== this.#subagentErrorBaseline
    const projection = this.#host.state.subagents[this.#activeSubagentId] ?? Object.values(
      this.#host.state.subagents,
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
      : history?.error ?? null
    this.#host.banner.visible = true
    this.#host.banner.fg = errorPresentation !== null
      ? this.#host.theme[errorPresentation.severity]
      : approval
        ? this.#host.theme.warning
        : this.#host.theme.info
    const childrenHint = this.#host.binding("open_subagent_picker")
    const paletteHint = this.#host.binding("open_command_picker")
    const hints = [
      "Esc parent",
      ...(childrenHint === null ? [] : [`${childrenHint} children`]),
      ...(paletteHint === null ? [] : [`${paletteHint} palette`]),
    ]
    this.#host.banner.content = t`${fg(this.#host.theme.primary)("◉ child agent")} · ${descriptor.task} · ${detail}${context === null ? "" : ` · ${context}`} · ${hints.join(" · ")}`
  }

  async interruptSubagent(subagentId: string): Promise<void> {
    const scope = this.#scope
    let outcome: void | CommandOutcome | null
    try {
      outcome = await this.#host.requests.emit({
        type: "interrupt_subagent",
        meta: this.#host.requests.meta(),
        session_id: this.#host.sessionId,
        subagent_id: subagentId,
      })
      if (scope !== this.#scope) return
    } catch (error) {
      if (scope !== this.#scope) return
      this.#host.projectError(
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
    if (outcome?.type === "rejected") this.#host.projectRejection(outcome)
    else if (outcome == null) {
      const presentation = presentError({
        category: "protocol",
        code: "subagent_interrupt_unavailable",
        message: "Couldn't interrupt the child because the engine connection is unavailable.",
      })
      this.#host.projectError(
        "subagent_interrupt_unavailable",
        presentation.text,
        true,
      )
    }
  }

  async closeSubagent(subagentId: string): Promise<void> {
    const scope = this.#scope
    let outcome: void | CommandOutcome | null
    try {
      outcome = await this.#host.requests.emit({
        type: "close_subagent",
        meta: this.#host.requests.meta(),
        session_id: this.#host.sessionId,
        subagent_id: subagentId,
      })
      if (scope !== this.#scope) return
    } catch (error) {
      if (scope !== this.#scope) return
      this.#host.projectError(
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
      this.#host.projectRejection(outcome)
      return
    }
    if (outcome == null) {
      const presentation = presentError({
        category: "protocol",
        code: "subagent_close_unavailable",
        message: "Couldn't close the child because the engine connection is unavailable.",
      })
      this.#host.projectError(
        "subagent_close_unavailable",
        presentation.text,
        true,
      )
      return
    }
    if (this.#activeSubagentId === subagentId) this.leaveSubagent()
    const { [subagentId]: _closed, ...subagents } = this.#host.state.subagents
    this.#host.state = {
      ...this.#host.state,
      subagents,
      subagentOrder: this.#host.state.subagentOrder.filter((candidate) => candidate !== subagentId),
    }
    this.#subagentDescriptors = this.#subagentDescriptors.filter(
      (subagent) => subagent.subagent_id !== subagentId,
    )
    this.draftStore.remove(`child:${subagentId}`)
    this.#host.refresh()
    this.requestSubagents()
  }

  render(kind: "agents" | "agentActions"): void {
    switch (kind) {
      case "agents": {
        if (this.#subagentListError !== null) {
          this.#host.pickerController.show(
            "Child agents · load failed",
            [{
              id: "agents.retry",
              label: "Retry loading child agents",
              description: boundedUiText(this.#subagentListError, 160),
              value: null,
            }],
            () => this.requestSubagents(),
          )
          break
        }
        if (
          this.#host.requests.current("subagents") !== null &&
          this.#subagentDescriptors.length === 0
        ) {
          this.#host.pickerController.showLoading("Child agents", "Loading child agents")
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
          this.#host.pickerController.showStatus(
            "Child agents",
            "No child agents",
            "Child agents started by this session will appear here.",
          )
          break
        }
        this.#host.pickerController.show("Child agents · Enter to inspect", items, (item) => {
          this.#host.closePicker()
          void this.enterSubagent(item.value.subagent_id)
        })
        break
      }
      case "agentActions": {
        const subagent = this.#subagentActionId === null
          ? undefined
          : this.subagentDescriptor(this.#subagentActionId)
        if (subagent === undefined) {
          this.#host.closePicker()
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
        this.#host.pickerController.show(`Child actions · ${boundedUiText(subagent.task, 64)}`, items, (item) => {
          const action = item.value
          if (action.kind === "running") return
          this.#host.closePicker()
          if (action.kind === "inspect") void this.enterSubagent(action.subagent.subagent_id)
          else if (action.kind === "continue") {
            void this.enterSubagent(action.subagent.subagent_id)
          } else if (action.kind === "interrupt") {
            void this.interruptSubagent(action.subagent.subagent_id)
          } else {
            void this.closeSubagent(action.subagent.subagent_id)
          }
        })
        break
      }
    }
  }
}
