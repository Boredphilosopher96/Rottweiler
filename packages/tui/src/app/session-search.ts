import type { SessionSearchMatch } from "../protocol"
import type { ClientCache } from "../history/cache"
import type { HistoryCacheValue } from "../history/controller"
import { retainedJsonBytes } from "../retained-json"

interface SearchHost {
  readonly historyCache: ClientCache<HistoryCacheValue>
  readonly sessionId: string
  readonly destroyed: boolean
  closePicker(): void
  selectSession(sessionId: string): void | Promise<void>
  navigateTranscript(source: SessionSearchMatch): Promise<unknown>
  projectError(code: string, message: string, retryable: boolean): void
}

/** One selected hit owns its source token across normal session activation and page resolution. */
export class SessionSearchNavigation {
  readonly #host: SearchHost
  #pending = false
  constructor(host: SearchHost) { this.#host = host }
  get pending(): boolean { return this.#pending }

  async open(source: SessionSearchMatch): Promise<void> {
    if (this.#host.destroyed) return
    if (this.#pending) {
      this.#host.projectError("navigation_pending", "A search navigation is already pending.", true)
      return
    }
    const allocations = this.#host.historyCache.allocations
    this.#pending = true
    try {
      // The picker and previous state may retire during activation. Retain the
      // selected token independently until the actual read has settled.
      using _source = allocations.reserve("live", retainedJsonBytes(source, allocations.limits.live))
      this.#host.closePicker()
      if (this.#host.sessionId !== source.session_id) await this.#host.selectSession(source.session_id)
      if (this.#host.destroyed) return
      if (this.#host.sessionId !== source.session_id) throw new Error("The matching session could not be opened.")
      await this.#host.navigateTranscript(source)
    } catch (error) {
      if (!this.#host.destroyed) this.#host.projectError("search_navigation_failed",
        error instanceof Error ? error.message : "The search match could not be opened.", true)
    } finally { this.#pending = false }
  }
}
