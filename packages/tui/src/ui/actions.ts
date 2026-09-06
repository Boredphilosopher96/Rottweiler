import type { ClientAllocationOwner, ClientAllocationLease } from "../client-allocation"
import type { CommandOutcome, UiActionRequest } from "../protocol"
import type { UiSurfaceModel } from "./presentation"

/** Exact source/panel revision stays charged while its action is in flight. */
export interface UiActionLease {
  readonly model: UiSurfaceModel
  readonly sessionId: string
  readonly target: UiActionRequest["target"]
  release(): void
}
interface UiActionOptions {
  readonly allocations: ClientAllocationOwner
  readonly allowed: (lease: UiActionLease) => boolean
  readonly execute: (session: string, request: UiActionRequest, allocation: ClientAllocationLease) => Promise<void | CommandOutcome | null>
  readonly changed: () => void
  readonly failed: (message: string) => void
}

/** Accepted mutations own their source through settlement even after the view closes. */
export class UiActionController {
  readonly #options: UiActionOptions
  #pending = false
  #scope: object = {}
  constructor(options: UiActionOptions) { this.#options = options }
  get pending(): boolean { return this.#pending }
  reset(): void { this.#scope = {} }

  async invoke(lease: UiActionLease, id: string): Promise<boolean> {
    using allocation = this.#options.allocations.reserve("decoding", 0)
    const scope = this.#scope
    let admitted = false
    try {
      const presentation = lease.model.presentation
      if (this.#pending || !this.#options.allowed(lease)
        || !presentation.descriptor.actions.some(action => action.id === id)) return false
      admitted = true
      this.#pending = true
      this.#options.changed()
      const result = await this.#options.execute(lease.sessionId, {
        owner: presentation.owner, contribution_id: presentation.descriptor.id,
        action_id: id, target: lease.target,
      }, allocation)
      if (scope !== this.#scope) return result?.type === "accepted"
      if (result?.type === "rejected") this.#options.failed(result.error.message)
      else if (result?.type !== "accepted") this.#options.failed("The engine did not acknowledge the action.")
      return result?.type === "accepted"
    } catch {
      if (scope === this.#scope) this.#options.failed("The action could not be delivered to the engine.")
      return false
    } finally {
      lease.release()
      if (admitted) {
        this.#pending = false
        this.#options.changed()
      }
    }
  }
}
