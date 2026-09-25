import { DRAFT_SWITCH_LIMIT_NOTICE } from "../render/resource-copy"
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
  agentsStripEntries,
  formatSubagentElapsed,
  type AgentsStripInput,
  type ComposerRenderable,
  type StateBannerRenderable,
} from "../components"
import type { ClientDiagnostics } from "../client-diagnostics"
import type { HistoryPresentation } from "../history/presentation"
import type { KeybindingAction } from "../keybindings"
import type { PickerController } from "../picker-controller"
import type { ProjectionRequestBroker } from "../projection-requests"
import type { CommandOutcome, EngineEvent } from "../protocol"
import { presentError } from "../render"
import { createInitialState, engineEvent, reduceRottweilerState, type RottweilerState, type SubagentProjection } from "../state"
import {
  boundSubagentState,
  childEngineEvent,
  foregroundSubagentId,
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
/** Child-agent controls the parent's driver sends on the user's behalf. */
type ChildControlCommand = "interrupt_subagent" | "close_subagent" | "background_subagent" | "continue_subagent"
const CONTROL_COPY: Readonly<Record<ChildControlCommand, { readonly code: string; readonly unavailable: string }>> = {
  interrupt_subagent: { code: "subagent_interrupt", unavailable: "Couldn't stop the agent because the engine connection is unavailable." },
  close_subagent: { code: "subagent_close", unavailable: "Couldn't close the agent because the engine connection is unavailable." },
  background_subagent: { code: "subagent_background", unavailable: "Couldn't move the agent to the background because the engine connection is unavailable." },
  continue_subagent: { code: "subagent_continue", unavailable: "Couldn't message the agent because the engine connection is unavailable." },
}
/** Bound on remembered strip membership; finished children beyond it are simply shown until dismissed. */
const MAX_STRIP_MEMORY = 512

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
  /** Children seen running in this client; only they persist in the strip once finished. */
  #stripObserved = new Set<string>()
  /** Finished children the user dismissed from the strip. */
  #stripHidden = new Set<string>()
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
  /**
   * Viewing a child never borrows the parent's composer: the view is read-only
   * unless the child waits on a typed answer. Messages go through the Agents screen.
   */
  get composerHidden(): boolean {
    if (this.#activeSubagentId === null) return false
    if (this.#historicalChild !== null || !this.familyControlReady) return true
    return !Object.values(this.#activeChildState?.questions ?? {}).some(question => question.question.response_kind === "text")
  }
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
    this.#historicalChild = null; this.#activeReadTarget = null; this.draftStore.clear(); this.#activeSubagentId = null
    this.#stripObserved.clear(); this.#stripHidden.clear()
    this.#subagentErrorBaseline = undefined
    this.#resetting = false
  }
  /** Accepts the engine's child catalog when it answers this session's latest request. */
  acceptListed(event: Extract<EngineEvent, { type: "subagents_listed" }>, requestId: string | null): void {
    if (event.session_id !== this.#host.sessionId || !this.#host.requests.matches("subagents", requestId)) return
    this.#host.requests.clear("subagents")
    this.acceptCatalog(event.subagents)
  }
  get catalog(): readonly SubagentDescriptor[] { return this.#subagentDescriptors }
  get familyPending(): readonly FamilyControlRow[] { return this.#family?.pending ?? [] }
  get listError(): string | null { return this.#subagentListError ?? this.#family?.error ?? null }
  get listLoading(): boolean { return this.#host.requests.current("subagents") !== null }
  retryListing(): void { this.#family?.refresh(); this.requestSubagents() }
  /** Child a foreground spawn or wait is blocked on; only it can move to the background. */
  get foregroundId(): string | null { return this.#host.state.replay.active ? null : foregroundSubagentId(this.#host.state) }

  /**
   * Refreshes the child catalog when a child starts, finishes, or a
   * `spawn_agent` call settles; queued children are known only from it.
   */
  afterEvent(event: EngineEvent, state: RottweilerState): void {
    if (event.type === "subagent_spawned" || event.type === "subagent_finished"
      || (event.type === "tool_call_finished" && state.tools[event.invocation_id]?.name === "spawn_agent")) this.requestSubagents()
  }

  /** Strip rows for the parent: running and queued children, then undismissed finished ones. */
  stripInput(agentsKey: string | null, backgroundKey: string | null): AgentsStripInput {
    const state = this.#host.state
    for (const subagent of Object.values(state.subagents)) {
      if (subagent.status === "running" && this.#stripObserved.size < MAX_STRIP_MEMORY) this.#stripObserved.add(subagent.projectionId)
    }
    for (const set of [this.#stripObserved, this.#stripHidden]) {
      for (const id of set) if (state.subagents[id] === undefined) set.delete(id)
    }
    const queued = new Set(this.#subagentDescriptors.filter(value => value.activity === "queued").map(value => value.subagent_id))
    const entries = agentsStripEntries(state, this.#stripHidden, this.#stripObserved)
    const waiting = this.#subagentDescriptors.filter(value => value.activity === "queued" && state.subagents[value.subagent_id] === undefined)
      .map(value => queuedProjection(value))
    const live = entries.filter(entry => entry.status === "running").length
    return {
      entries: [...entries.slice(0, live), ...waiting, ...entries.slice(live)],
      agentName: id => this.subagentDescriptor(id)?.agent || null,
      queued: id => queued.has(id),
      agentsKey,
      backgroundKey: this.foregroundId === null ? null : backgroundKey,
    }
  }
  /** Finished children currently listed in the strip. */
  get finishedInStrip(): number {
    return Object.values(this.#host.state.subagents).filter(subagent => subagent.status !== "running"
      && this.#stripObserved.has(subagent.projectionId) && !this.#stripHidden.has(subagent.projectionId)).length
  }
  /** Removes finished children from the strip; the Agents screen still lists them. */
  hideFinished(): void {
    for (const subagent of Object.values(this.#host.state.subagents)) {
      if (subagent.status !== "running" && this.#stripHidden.size < MAX_STRIP_MEMORY) this.#stripHidden.add(subagent.projectionId)
    }
    this.#host.refresh()
  }

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
    this.#subagentErrorBaseline = undefined
    this.#host.refresh(); source?.release()
    this.#host.focus()
  }

  saveComposerDraft(): boolean {
    const accepted = this.draftStore.set(this.composerScope(), {
      content: this.#host.composer.value, attachments: this.#host.composer.attachments,
    })
    if (!accepted) this.#host.projectError("draft_budget_full", DRAFT_SWITCH_LIMIT_NOTICE)
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
      // The active approval owns the foreground banner until it is resolved.
      if (Object.values(state.tools).some(tool => tool.status === "awaiting_approval")) return
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
    // Viewing a child overlays the parent, which keeps running underneath.
    const subagentId = this.#activeSubagentId
    const descriptor = this.subagentDescriptor(subagentId)
    const projection = this.#host.state.subagents[subagentId] ?? Object.values(
      this.#host.state.subagents,
    ).findLast((subagent) => subagent.subagentId === subagentId)
    const name = descriptor?.agent || "agent"
    const approval = Object.values(state.tools).some((tool) => tool.status === "awaiting_approval")
    const history = this.#host.history?.controller.snapshot
    const replaying = history?.loading ?? false
    const latestError = this.#host.state.errors.at(-1)
    const hasErrorContext = latestError !== undefined && latestError !== this.#subagentErrorBaseline
    const status = this.#historicalChild !== null
      ? "history"
      : projection?.status.replaceAll("_", " ") ?? descriptor?.activity ?? "loading"
    const elapsed = projection?.status === "running" ? formatSubagentElapsed(projection.spawnedAtMs) : null
    const activity = replaying
      ? "loading transcript"
      : approval
        ? "needs your approval"
        : this.#familyChild !== null && !this.familyControlReady
          ? "refreshing controls"
          : projection?.status === "running" ? projection.activity ?? "" : ""
    const errorPresentation = hasErrorContext && latestError !== undefined ? presentError(latestError) : null
    const context = errorPresentation?.text ?? history?.error ?? this.#family?.error ?? this.#displayError ?? null
    const parentWaiting = Object.values(this.#host.state.tools).some(tool => tool.status === "awaiting_approval")
      || Object.keys(this.#host.state.questions).length > 0 || this.#host.state.pendingPlan !== null
    const detail = [
      status,
      ...(activity.trim() === "" || activity.trim().toLowerCase() === status.toLowerCase() ? [] : [activity.trim()]),
      ...(elapsed === null ? [] : [elapsed]),
      ...(context === null ? [] : [context]),
      ...(parentWaiting ? ["parent needs you"] : []),
    ].join(" · ")
    this.#host.banner.visible = true
    this.#host.banner.fg = errorPresentation !== null
      ? this.#host.theme[errorPresentation.severity]
      : approval || parentWaiting ? this.#host.theme.warning : this.#host.theme.info
    const task = this.#historicalChild?.task ?? projection?.task ?? descriptor?.task ?? ""
    this.#host.banner.content = t`${fg(this.#host.theme.primary)(`Agent · ${name}`)} · ${detail} · Esc back${task === "" ? "" : ` · ${boundedUiText(task, 96)}`}`
  }

  async interruptSubagent(subagentId: string): Promise<boolean> {
    return await this.#control("interrupt_subagent", subagentId)
  }

  /** Detaches the child a foreground spawn or wait is blocked on; the parent continues. */
  async backgroundSubagent(subagentId: string): Promise<boolean> {
    return await this.#control("background_subagent", subagentId)
  }

  /** Sends an explicit follow-up to a finished child. */
  async messageSubagent(subagentId: string, content: string): Promise<boolean> {
    const accepted = await this.#control("continue_subagent", subagentId, content)
    if (accepted) this.responseStarted(subagentId)
    return accepted
  }

  async closeSubagent(subagentId: string): Promise<boolean> {
    const scope = this.#scope
    if (!await this.#control("close_subagent", subagentId) || scope !== this.#scope) return false
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
    return true
  }

  async #control(type: ChildControlCommand, subagentId: string, content?: string): Promise<boolean> {
    using replyAllocation = this.#host.requests.allocate()
    const scope = this.#scope
    const copy = CONTROL_COPY[type]
    let outcome: void | CommandOutcome | null
    try {
      const target = { meta: this.#host.requests.meta(), session_id: this.#host.sessionId, subagent_id: subagentId }
      outcome = await this.#host.requests.emit(type === "continue_subagent"
        ? { type, ...target, content: content ?? "" }
        : { type, ...target }, replyAllocation)
    } catch (error) {
      if (scope === this.#scope) {
        this.#host.projectError(`${copy.code}_failed`, presentError({
          category: "protocol", code: `${copy.code}_failed`, message: safeErrorMessage(error),
        }).text, true)
      }
      return false
    }
    if (scope !== this.#scope) return false
    if (outcome?.type === "rejected") this.#host.projectRejection(outcome)
    else if (outcome == null) {
      this.#host.projectError(`${copy.code}_unavailable`, presentError({
        category: "protocol", code: `${copy.code}_unavailable`, message: copy.unavailable,
      }).text, true)
    }
    return outcome?.type === "accepted"
  }
}

/** Strip row for a child admitted by the parent but still waiting for a slot. */
function queuedProjection(descriptor: SubagentDescriptor): SubagentProjection {
  return {
    projectionId: descriptor.subagent_id, subagentId: descriptor.subagent_id, parentTurnId: "",
    task: descriptor.task, spawnedAtMs: null, status: "running", childSessionId: descriptor.child_session_id,
    lastChildSequence: null, activity: "queued", summary: null, touchedFileCount: 0, diffArtifactId: null,
  }
}
