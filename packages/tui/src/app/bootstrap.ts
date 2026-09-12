import type { RottweilerState } from "../state"
import type { SessionBootstrap } from "../runtime-bootstrap"

/** The renderer handoff retains old and incoming allocations until all component references change. */
export class BootstrapPresentation {
  #current: SessionBootstrap | null = null
  #failed: SessionBootstrap | null = null
  #disposed = false
  constructor(readonly replace: (state: RottweilerState) => void) {}
  install(incoming: SessionBootstrap): void {
    if (this.#disposed) { incoming.release(); throw new DOMException("application is destroyed", "AbortError") }
    if (this.#failed !== null) { incoming.release(); throw new Error("bootstrap presentation requires disposal after a failed rebind") }
    const previous = this.#current
    // If rebinding throws, both owners stay charged: partially updated components may reference either.
    const next = incoming.takeState()
    this.#failed = incoming
    this.replace(next)
    this.#failed = null
    this.#current = incoming
    previous?.release()
  }
  /** Call only after models, pending presentation and component references have been cleared. */
  dispose(): void { this.#disposed = true; this.#current?.release(); this.#current = null; this.#failed?.release(); this.#failed = null }
}
