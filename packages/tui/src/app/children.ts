import { retainedJsonBytes } from "../retained-json"
import type { RecycleChildTarget } from "../recycle-child"
import { SubagentCatalog } from "../subagent-catalog"
import { ChildDisplayController } from "../child-display"
import { readSessionState } from "../state/recovery"
import { installLiveTail } from "../state/tail-recovery"
import { FamilyControlsController } from "../family-controls"
import { sameChildTarget, type FamilyControlsReader } from "../family-controls-reader"
import { resolveFamilyHistory } from "../family-history"
import type { ChildControlResponse, ChildControlTarget, FamilyControlRow } from "../../../../protocol/types"
import { readControls, resolvedApproval } from "../state/controls"
import { childPassiveInteractionState } from "../subagent-state"
import type { TranscriptRenderable } from "../components"
import type { ProjectionAllocations } from "../state/allocation"
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
  type ComposerDraft,
  type SubagentDescriptor,
} from "../subagent-state"
import type { RottweilerTheme } from "../theme"
import { boundedUiText } from "../ui-presentation"
interface ChildUiHost {
  readonly allocations: ProjectionAllocations
  state: RottweilerState
  readonly sessionId: string
  readonly composer: ComposerRenderable
  readonly banner: StateBannerRenderable
  readonly theme: RottweilerTheme
  readonly history: HistoryPresentation
  readonly diagnostics: ClientDiagnostics | undefined
  readonly pickerController: PickerController
  readonly requests: ProjectionRequestBroker
  readonly familyControls: FamilyControlsReader | undefined
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

export class ChildUiController {
  readonly #host: ChildUiHost
  #scope: object = {}
  #resetting = false
  #subagentListError: string | null = null
  readonly #catalog: SubagentCatalog
  get #subagentDescriptors(): readonly SubagentDescriptor[] { return this.#catalog.values }
  get #activeChildState(): RottweilerState | null { return this.#host.allocations.child }
  set #activeChildState(value: RottweilerState | null) { this.#host.allocations.set("child", value) }
  #historicalChild: { readonly sessionId: string; readonly task: string; readonly target: SessionReadTarget } | null = null
  readonly draftStore: ComposerDraftStore
  #activeSubagentId: string | null = null
  #subagentActionId: string | null = null
  #subagentErrorBaseline: RottweilerState["errors"][number] | undefined
  #parentReadTarget: SessionReadTarget | null = null
  #activeReadTarget: SessionReadTarget | null = null
  readonly #family: FamilyControlsController | null
  #familyChild: ChildControlTarget | null = null
  #sourceRequest: AbortController | null = null
  #sourceOwner: { target: SessionReadTarget; release(): void } | null = null
  #sourceError: string | null = null
  #wasConnected = false
  #needsSource = false
  readonly #display: ChildDisplayController | null
  #displaySource: SessionReadTarget | null = null
  #displayTarget: ChildControlTarget | null = null
  #displayError: string | null = null
  readonly #todos: TodoController
  constructor(host: ChildUiHost) {
    this.#catalog = new SubagentCatalog(host.history.controller.cache.allocations)
    this.draftStore = new ComposerDraftStore(undefined, undefined, host.history.controller.cache.allocations)
    this.#host = host
    this.#family = host.familyControls === undefined ? null : new FamilyControlsController({
      allocations: host.history.controller.cache.allocations, reader: host.familyControls,
      changed: () => host.refresh(),
      apply: snapshot => {
        const state = this.#activeChildState
        if (state === null) return
        this.#activeChildState = snapshot === null ? { ...state, questions: {}, pendingPlan: null,
          tools: Object.fromEntries(Object.entries(state.tools).map(([id, tool]) => [id, tool.status === "awaiting_approval" ? resolvedApproval(tool) : tool])),
        } : readControls(state, snapshot)
      },
    })
    this.#display = host.familyControls === undefined ? null : new ChildDisplayController({
      cache: host.history.controller.cache,
      readState: (root, target, signal, allocation) => host.familyControls!.state(root, target, signal, allocation),
      readTail: (target, read, signal, allocation) => host.sessionReader.tail(target, read, signal, allocation),
      apply: (snapshot, pages) => {
        const current = this.#activeChildState
        if (current === null || this.#familyChild === null) return
        let next = readSessionState(current, this.#familyChild.session_id, snapshot)
        if (pages !== null) {
          next = installLiveTail(next, pages)
          next = { ...next, tools: { ...next.tools, ...Object.fromEntries(Object.entries(current.tools).filter(([, tool]) => tool.status === "awaiting_approval")) } }
        }
        this.#activeChildState = next
        if (pages !== null) {
          host.history.invalidate(this.#familyChild.session_id)
          this.#todos.open(this.readTarget, snapshot.through)
        }
        host.refresh()
      },
      failed: message => { if (message !== this.#displayError) { this.#displayError = message; host.refresh() } },
    })
    this.#todos = new TodoController({
      allocations: host.history.controller.cache.allocations,
      reader: host.sessionReader,
      state: () => this.#activeChildState?.todos ?? emptyTodos(),
      update: todos => {
        if (this.#activeChildState === null) return
        this.#activeChildState = { ...this.#activeChildState, todos }
        this.#host.refresh()
      },
    })
  }
  syncFamily(): void {
    if (this.#resetting) return
    const connected = this.#host.state.connection.phase === "connected" && !this.#host.state.replay.active
    if (!connected && this.#wasConnected) {
      this.#sourceRequest?.abort(); this.#sourceRequest = null
      this.#needsSource = this.#activeSubagentId !== null
    }
    this.#wasConnected = connected
    this.#family?.connect(connected ? this.#host.sessionId : null)
    if (this.#activeSubagentId !== null && this.#familyChild === null && this.#historicalChild === null) {
      const row = this.#family?.rows.find(value => value.target.ancestry.at(-1)?.subagent_id === this.#activeSubagentId)
      if (row !== undefined) {
        this.#familyChild = row.target
        this.#family?.select(row.target)
        this.#familyChild = this.#family?.target ?? null
      }
    }
    if (connected && this.#needsSource && this.#familyChild !== null
      && this.#family?.rows.some(row => sameChildTarget(row.target, this.#familyChild!) && row.controls.available)) {
      this.#needsSource = false; this.#loadSource(this.#familyChild)
    }
    this.#syncDisplay()
  }
  #syncDisplay(): void {
    const target = this.#familyChild, source = this.#activeReadTarget
    const available = target !== null && this.#family?.rows.some(row => sameChildTarget(row.target, target) && row.controls.available)
    if (this.#host.state.connection.phase !== "connected" || this.#sourceRequest !== null || this.#needsSource || !available || target === null || source === null) {
      this.#display?.close(); this.#displaySource = null; this.#displayTarget = null
      return
    }
    if (this.#displaySource === source && this.#displayTarget === target) return
    this.#display?.open(this.#host.sessionId, target, source)
    this.#displaySource = source; this.#displayTarget = target
  }
  get controlsPending(): boolean { return this.#family?.pendingResponses === true }
  get sourceReady(): boolean { return this.#activeSubagentId === null || this.#activeReadTarget !== null }
  get familyControlReady(): boolean { return this.#familyChild !== null && this.#family?.ready === true }
  get selectedFamily(): boolean { return this.#familyChild !== null }
  interactionState(state: RottweilerState): RottweilerState { return this.#activeSubagentId === null || this.familyControlReady ? state : childPassiveInteractionState(state) }
  presentHistory(transcript: TranscriptRenderable): void {
    if (this.sourceReady) this.#host.history.present(this.readTarget)
    else this.#host.history.suspend()
    const snapshot = this.#host.history.controller.snapshot
    transcript.setHistory(this.sourceReady ? snapshot : { ...snapshot, page: null, total: 0n, loading: true, error: this.#sourceError, selection: null, anchor: null })
  }
  async respond(response: ChildControlResponse): Promise<boolean> {
    const family = this.#family
    if (family === null || this.#familyChild === null) return false
    const scope = this.#scope, selected = this.#familyChild
    using allocation = this.#host.requests.allocate()
    try {
      const outcome = await family.respond(response, async (session_id, target, expected_revision, response) => {
        return await this.#host.requests.emit({ type: "resolve_child_control", meta: this.#host.requests.meta(), session_id, target, expected_revision, response }, allocation)
      })
      if (scope !== this.#scope || selected !== this.#familyChild) return outcome?.type === "accepted"
      if (outcome?.type === "rejected") this.#host.projectRejection(outcome)
      return outcome?.type === "accepted"
    } catch (error) { if (scope === this.#scope && selected === this.#familyChild) this.#host.projectError("child_control_failed", safeErrorMessage(error), true); return false }
  }
  enterFamily(row: FamilyControlRow): void {
    if (!this.saveComposerDraft()) return
    const previousTarget = this.#familyChild
    try { this.#familyChild = row.target; this.#family?.select(row.target) }
    catch (error) { this.#familyChild = previousTarget; this.#host.projectError("child_control_admission", safeErrorMessage(error), true); return }
    this.#familyChild = this.#family?.target ?? null
    this.#sourceRequest?.abort()
    const prior = this.#sourceOwner; this.#sourceOwner = null
    this.#activeSubagentId = row.target.ancestry.at(-1)!.subagent_id
    this.#historicalChild = null; this.#activeReadTarget = null; this.#sourceError = null
    this.#activeChildState = { ...createInitialState(), connection: { ...createInitialState().connection, phase: "connected" } }
    this.restoreComposerDraft(this.#activeSubagentId)
    this.#host.refresh(); this.#host.focus(); prior?.release()
    this.#loadSource(row.target)
  }
  #loadSource(target: ChildControlTarget): void {
    this.#sourceRequest?.abort()
    const request = new AbortController(); this.#sourceRequest = request
    this.#sourceError = null
    void this.#resolveHistory(target, request.signal).then(source => {
      if (request.signal.aborted) { source.release(); return }
      this.#sourceRequest = null
      const previous = this.#sourceOwner
      this.#sourceOwner = source; this.#activeReadTarget = source.target
      this.#todos.open(source.target)
      try { this.#host.refresh() } finally { previous?.release() }
    }).catch(error => { if (!request.signal.aborted) { this.#sourceError = safeErrorMessage(error); this.#host.refresh() } })
      .finally(() => { if (this.#sourceRequest === request) this.#sourceRequest = null })
  }

  #resolveHistory(target: ChildControlTarget, signal: AbortSignal) {
    if (this.#host.familyControls === undefined) return Promise.reject(new Error("Live child history authority is unavailable."))
    return resolveFamilyHistory(this.#host.familyControls, this.#host.history.controller.cache.allocations, this.#host.sessionId, target, signal)
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
  captureRecycleTarget(): RecycleChildTarget | null {
    if (this.#familyChild !== null) return { type: "live", target: this.#familyChild }
    if (this.#historicalChild !== null) return { type: "historical", target: this.#historicalChild.target }
    return null
  }
  restoreRecycleTarget(saved: RecycleChildTarget): boolean {
    if (saved.type === "live") {
      if (this.#familyChild !== null && sameChildTarget(saved.target, this.#familyChild)) return this.sourceReady && this.familyControlReady
      const row = this.#family?.rows.find(row => sameChildTarget(row.target, saved.target) && row.controls.available)
      if (row === undefined) return false
      this.enterFamily(row)
      return false
    }
    if (this.#historicalChild?.target === saved.target) return true
    if (!this.saveComposerDraft() || saved.target.scope.type !== "descendant") return false
    this.#family?.select(null); this.#familyChild = null; this.#sourceRequest?.abort()
    const allocation = this.#host.history.controller.cache.allocations.reserve("children", retainedJsonBytes(saved.target, 65536))
    const previous = this.#sourceOwner
    this.#sourceOwner = { target: saved.target, release: () => allocation.release() }
    this.#activeReadTarget = saved.target
    previous?.release()
    this.#historicalChild = { sessionId: saved.target.sessionId, task: "Child history", target: saved.target }
    this.#activeSubagentId = saved.target.scope.ancestry.at(-1)!.subagent_id
    this.#activeChildState = createInitialState()
    this.#todos.open(saved.target); this.restoreComposerDraft(this.#activeSubagentId)
    this.#host.refresh(); this.#host.focus()
    return true
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
    this.#resetting = true
    this.#wasConnected = false; this.#needsSource = false
    this.#display?.close(); this.#displaySource = null; this.#displayTarget = null; this.#displayError = null
    this.#familyChild = null; this.#family?.close()
     this.#sourceRequest?.abort(); this.#sourceRequest = null
    this.#sourceOwner?.release(); this.#sourceOwner = null; this.#sourceError = null
    this.#todos.reset()
    this.#scope = {}
    this.#subagentListError = null; this.#catalog.clear(); this.#activeChildState = null
    this.#historicalChild = null; this.#activeReadTarget = null; this.draftStore.clear(); this.#activeSubagentId = null; this.#subagentActionId = null
    this.#subagentErrorBaseline = undefined
    this.#resetting = false
  }
  pickerClosed(): void { this.#subagentActionId = null }
  acceptCatalog(values: readonly SubagentDescriptor[]): void {
    this.#subagentListError = null
    this.#catalog.replace(values)
    if (this.#familyChild === null && this.#activeSubagentId !== null && this.subagentDescriptor(this.#activeSubagentId) === undefined && this.#historicalChild === null) this.leaveSubagent()
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
    this.#family?.select(null); this.#familyChild = null; this.#sourceRequest?.abort()
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
    void this.#host.requests.consume({
      type: "list_subagents",
      meta,
      session_id: this.#host.sessionId,
    }, (outcome) => {
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
    const family = this.#family?.rows.find(row => row.target.ancestry.at(-1)?.subagent_id === subagentId)
    if (family !== undefined) { this.enterFamily(family); return }
    const descriptor = this.subagentDescriptor(subagentId)
    if (descriptor === undefined) return
    if (!this.saveComposerDraft()) return
    this.#activeSubagentId = subagentId
    this.#historicalChild = null
    this.restoreComposerDraft(subagentId)
    this.#subagentErrorBaseline = this.#host.state.errors.at(-1)
    this.#activeChildState = initialSubagentState(this.#host.state, descriptor)
    this.#activeReadTarget = null
    this.#loadSource({ session_id: descriptor.child_session_id, ancestry: [{ subagent_id: subagentId, session_id: descriptor.child_session_id }] })
    this.#host.refresh()
    this.#host.focus()
  }

  leaveSubagent(): void {
    if (this.#activeSubagentId === null) return
    if (!this.saveComposerDraft()) return
    this.#family?.select(null); this.#familyChild = null
    this.#display?.close(); this.#displaySource = null; this.#displayTarget = null; this.#displayError = null
    this.#sourceRequest?.abort(); this.#sourceRequest = null
    const source = this.#sourceOwner; this.#sourceOwner = null
    this.#needsSource = false
    this.#todos.reset()
    this.#activeSubagentId = null
    this.#activeReadTarget = null
    this.#historicalChild = null
    this.#activeChildState = null
    this.restoreComposerDraft(null)
    this.#subagentActionId = null
    this.#subagentErrorBaseline = undefined
    this.#host.refresh(); source?.release()
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
    if (this.#familyChild?.session_id === event.child_session_id) {
      this.#host.history.invalidate(event.child_session_id)
      return true
    }
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
    const current = this.#activeChildState ?? initialSubagentState(this.#host.state, descriptor)
    this.#activeChildState = { ...initialSubagentState(this.#host.state, descriptor), lastSequence: sequence,
      questions: current.questions, pendingPlan: current.pendingPlan, controls: current.controls,
      tools: Object.fromEntries(Object.entries(current.tools).filter(([, tool]) => tool.status === "awaiting_approval")),
    }
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
    if (this.#familyChild !== null) return !this.familyControlReady || (!Object.values(this.#activeChildState?.questions ?? {}).some(question => question.question.response_kind === "text") && this.subagentDescriptor(this.#activeSubagentId!)?.activity !== "idle")
    return this.#activeSubagentId !== null &&
      this.subagentDescriptor(this.#activeSubagentId)?.activity === "running"
  }

  setSubagentActivity(subagentId: string, activity: SubagentDescriptor["activity"]): void {
    this.#catalog.activity(subagentId, activity)
  }

  presentedState(): RottweilerState {
    if (this.#activeSubagentId === null) return this.#host.state
    if (this.#activeChildState !== null) return this.#activeChildState
    const descriptor = this.subagentDescriptor(this.#activeSubagentId)
    if (descriptor === undefined) return this.#host.state
    return this.#activeChildState ?? initialSubagentState(this.#host.state, descriptor)
  }

  updateSubagentBanner(state: RottweilerState): void {
    if (this.#activeSubagentId === null) {
      const pending = this.#family?.pending.length ?? 0
      if (this.#family?.error !== null && this.#family?.error !== undefined) {
        this.#host.banner.visible = true; this.#host.banner.fg = this.#host.theme.warning
        this.#host.banner.content = `Child controls unavailable · ${this.#host.binding("open_subagent_picker") ?? "/agents"} retry`
        return
      }
      if (pending > 0) {
        this.#host.banner.visible = true; this.#host.banner.fg = this.#host.theme.warning
        this.#host.banner.content = `${pending} child ${pending === 1 ? "agent needs" : "agents need"} a response · ${this.#host.binding("open_subagent_picker") ?? "/agents"} inspect`
      }
      return
    }
    if (this.#familyChild !== null) {
      this.#host.banner.visible = true
      this.#host.banner.content = `Child ${this.#familyChild.session_id} · ${this.familyControlReady ? "controls ready" : "refreshing controls"}${this.#family?.error ? ` · ${this.#family.error}` : ""}${this.#displayError ? ` · ${this.#displayError}` : ""} · Esc parent`
      return
    }
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
    using replyAllocation = this.#host.requests.allocate()
    const scope = this.#scope
    let outcome: void | CommandOutcome | null
    try {
      outcome = await this.#host.requests.emit({
        type: "interrupt_subagent",
        meta: this.#host.requests.meta(),
        session_id: this.#host.sessionId,
        subagent_id: subagentId,
      }, replyAllocation)
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
    using replyAllocation = this.#host.requests.allocate()
    const scope = this.#scope
    let outcome: void | CommandOutcome | null
    try {
      outcome = await this.#host.requests.emit({
        type: "close_subagent",
        meta: this.#host.requests.meta(),
        session_id: this.#host.sessionId,
        subagent_id: subagentId,
      }, replyAllocation)
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
    this.#catalog.remove(subagentId)
    this.draftStore.remove(`child:${subagentId}`)
    this.#host.refresh()
    this.requestSubagents()
  }

  render(kind: "agents" | "agentActions"): void {
    switch (kind) {
      case "agents": {
        const listingError = this.#subagentListError ?? this.#family?.error ?? null
        if (listingError !== null && (this.#family?.pending.length ?? 0) === 0) {
          this.#host.pickerController.show(
            "Child agents · load failed",
            [{
              id: "agents.retry",
              label: "Retry loading child agents",
              description: boundedUiText(listingError, 160),
              value: null,
            }],
            () => { this.#family?.refresh(); this.requestSubagents() },
          )
          break
        }
        if (
          this.#host.requests.current("subagents") !== null &&
          this.#subagentDescriptors.length === 0 && (this.#family?.pending.length ?? 0) === 0
        ) {
          this.#host.pickerController.showLoading("Child agents", "Loading child agents")
          break
        }
        type Choice = { agent: SubagentDescriptor } | { control: FamilyControlRow }
        const pending = this.#family?.pending ?? []
        const items: PickerItem<Choice>[] = pending.map(row => ({ id: `control:${row.target.session_id}`,
          label: `Response needed · ${row.target.session_id}`,
          description: `${row.controls.questions} questions · ${row.controls.approvals} approvals${row.controls.pending_plan ? " · plan review" : ""}`,
          value: { control: row },
        }))
        items.push(...this.#subagentDescriptors.filter(subagent => !pending.some(row => row.target.session_id === subagent.child_session_id)).map((subagent) => ({
          id: subagent.subagent_id,
          label: subagent.task,
          description: `${subagent.activity === "running" ? "Running" : "Idle"} · ${subagent.agent} · ${subagent.model} · ${subagent.isolation}`,
          searchText: `${subagent.task} ${subagent.agent} ${subagent.model} ${subagent.activity}`,
          value: { agent: subagent },
        })))
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
          if ("control" in item.value) this.enterFamily(item.value.control)
          else void this.enterSubagent(item.value.agent.subagent_id)
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
