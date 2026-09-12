import type { ReviewPanelRenderable } from "../components/review-panel"
import type { ClientAllocationLease, ClientAllocationOwner } from "../client-allocation"
import type { EngineEvent } from "../protocol"
import type { RottweilerState } from "../state"
import { reviewRoots, type RecycleReview } from "../recycle-review"
import type { ProjectionRequestBroker } from "../projection-requests"

interface Host {
  readonly panel: ReviewPanelRenderable
  readonly state: RottweilerState
  readonly sessionId: string
  readonly requests: ProjectionRequestBroker
  readonly allocations: ClientAllocationOwner
  opened(): void
  failed(code: string, message: string): void
}
interface Pending {
  readonly state: RecycleReview
  readonly sessionId: string
  requestId: string | null
  received: string | null
  laidOut: boolean
  readonly allocation: ClientAllocationLease
}

/** Fresh read authority precedes all restored review actions and viewport adoption. */
export class ReviewController {
  #pending: Pending | null = null
  constructor(readonly host: Host) {}
  get pending(): boolean { return this.#pending !== null }
  open(mode: RecycleReview["mode"], path = ""): void {
    this.close()
    if (this.host.state.replay.active) return
    if (this.host.state.shell.active) {
      if (mode === "session") this.host.failed("review_unavailable_during_shell", "exit the foreground shell before opening session review")
      return
    }
    this.present(mode, path, "Loading changed-file diff…")
    this.host.requests.command(mode === "session" ? { type: "get_session_review" }
      : { type: "get_workspace_diff", path, max_bytes: 1_000_000 })
  }
  private present(mode: RecycleReview["mode"], path: string, message: string): void {
    if (mode === "session") this.host.panel.showSessionReview()
    else this.host.panel.showWorkspaceDiffMessage(path, message)
    this.host.opened()
  }
  begin(state: RecycleReview): void {
    this.close()
    const allocation = this.host.allocations.reserve("drafts", 16 * 1024)
    this.#pending = { state, sessionId: this.host.sessionId, requestId: null, received: null, laidOut: false, allocation }
    try {
      this.host.panel.setRestorePending(true)
      this.present(state.mode, state.path, "Revalidating changed-file diff…")
      const request = state.mode === "session" ? { type: "get_session_review" as const }
        : { type: "get_workspace_diff" as const, path: state.path, max_bytes: 1_000_000 }
      this.#pending.requestId = this.host.requests.command(request)
      if (this.#pending.requestId === null) this.fail("Review source could not be requested")
    } catch (error) { this.close(); throw error }
  }
  observe(event: EngineEvent): void {
    const pending = this.#pending
    if (pending === null || !((pending.state.mode === "session" && event.type === "session_review_ready")
      || (pending.state.mode === "workspace" && event.type === "workspace_diff_ready"))) return
    if (event.session_id === pending.sessionId) pending.received = event.meta.request_id
  }
  rejected(requestId: string, message: string): void {
    if (this.#pending?.requestId === requestId) this.fail(message)
  }
  apply(): void {
    const pending = this.#pending
    if (pending === null || pending.requestId === null || pending.received !== pending.requestId) return
    if (pending.sessionId !== this.host.sessionId) { this.close(); return }
    if (!this.host.panel.validateRestoredSource(pending.state, reviewRoots(this.host.state))) {
      this.fail("The reviewed source changed or disappeared; reopen review to inspect its current content")
      return
    }
    // Selecting the restored file dirties its native diff layout. Adopt the
    // viewport only on the following frame, after that layout has run.
    if (!pending.laidOut) { pending.laidOut = true; return }
    if (this.host.panel.diffScroller.viewport.height === 0) return
    this.host.panel.restoreViewport(pending.state)
    this.close()
  }
  fail(message: string): void {
    const state = this.#pending?.state
    this.close()
    if (state === undefined) return
    this.host.panel.showWorkspaceDiffMessage(state.path, message)
    this.host.failed("review_restore_source_changed", message)
  }
  close(): void {
    const pending = this.#pending
    this.#pending = null
    this.host.panel.setRestorePending(false)
    pending?.allocation.release()
  }
}
