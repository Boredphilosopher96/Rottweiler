import { MAX_UI_DESCRIPTOR_BYTES, MAX_UI_PANELS_BYTES, type UiCatalogEntry, type UiPresentation } from "../protocol"
import type { SessionReader } from "../session-reader"
import type { ClientCache, CacheLease } from "../history/cache"
import type { HistoryCacheValue } from "../history/controller"
import type { UiActionLease } from "./actions"
import { prepareUiPanels, uiIdentity, type UiPanelModel } from "./presentation"

// Closed field schemas exclude arbitrary tiny object graphs. Account for encoded
// collection, UTF-16 decode, retained fields and prepared native strings together.
export const UI_COLLECTION_ALLOCATION_FACTOR = 16
const POLL_MILLIS = 1000
interface CatalogSnapshot {
  readonly sessionId: string | null
  readonly entries: readonly UiCatalogEntry[]
  readonly panels: readonly UiPanelModel[]
  readonly loading: boolean
  readonly error: string | null
}

/** One visible session's catalog and panel revisions share the content cache. */
export class UiCatalogController {
  readonly #reader: Pick<SessionReader, "uiCatalog" | "uiPanels">
  readonly #cache: ClientCache<HistoryCacheValue>
  readonly #changed: () => void
  #session: string | null = null
  #mode: "catalog" | "panels" = "catalog"
  #catalog: CacheLease<HistoryCacheValue> | null = null
  #panels: CacheLease<HistoryCacheValue> | null = null
  #requests = new Map<"catalog" | "panels", AbortController>()
  #timer: ReturnType<typeof setTimeout> | null = null
  #error: string | null = null
  #generation = 0
  #refreshing = false

  constructor(reader: Pick<SessionReader, "uiCatalog" | "uiPanels">, cache: ClientCache<HistoryCacheValue>, changed: () => void) {
    this.#reader = reader; this.#cache = cache; this.#changed = changed
  }
  get snapshot(): CatalogSnapshot {
    const catalog = this.#catalog?.value, panels = this.#panels?.value
    return { sessionId: this.#session, entries: catalog?.kind === "ui_catalog" ? catalog.catalog.entries : [],
      panels: panels?.kind === "ui_panels" ? panels.panels : [], loading: this.#refreshing, error: this.#error }
  }
  open(session: string, mode: "catalog" | "panels"): void {
    this.close()
    this.#session = session; this.#mode = mode
    this.refresh()
  }
  refresh(): void {
    if (this.#session === null || this.#refreshing) return
    if (this.#timer !== null) clearTimeout(this.#timer)
    this.#timer = null; this.#error = null
    this.#refreshing = true
    void this.#refresh(this.#generation)
  }
  async #refresh(generation: number): Promise<void> {
    try {
      // Sequential reservations leave room for the currently mounted revision.
      // Read concurrency remains shared with every other client feature.
      await this.#read("catalog")
      if (generation === this.#generation && this.#error === null && this.#mode === "panels") await this.#read("panels")
    } finally {
      if (generation === this.#generation) {
        this.#refreshing = false
        this.#changed()
        if (this.#session !== null && this.#error === null) {
          this.#timer = setTimeout(() => { this.#timer = null; this.refresh() }, POLL_MILLIS)
        }
      }
    }
  }
  close(): void {
    this.#generation++
    this.#session = null
    this.#refreshing = false
    for (const request of this.#requests.values()) request.abort()
    this.#requests.clear()
    if (this.#timer !== null) clearTimeout(this.#timer)
    this.#timer = null; this.#error = null
    const catalog = this.#catalog, panels = this.#panels
    this.#catalog = null; this.#panels = null
    try { this.#changed() } finally { catalog?.release(); panels?.release() }
  }
  current(presentation: UiPresentation): boolean {
    if (this.#error !== null) return false
    const key = uiIdentity(presentation)
    const entry = this.snapshot.entries.find(entry => uiIdentity(entry) === key)
    return entry !== undefined && entry.descriptor.surface.surface === presentation.descriptor.surface.surface
      && presentation.descriptor.actions.every(action => entry.descriptor.actions.some(current => current.id === action.id))
  }
  pinPanel(identity: string): UiActionLease | null {
    if (this.#session === null) return null
    const lease = this.#cache.lease(this.#key("panels"))
    if (lease === null) return null
    const value = lease.value
    const index = value.kind === "ui_panels" ? value.panels.findIndex(panel => uiIdentity(panel.model.presentation) === identity) : -1
    if (value.kind !== "ui_panels" || index < 0) { lease.release(); return null }
    const revision = value.panels[index]!.revision
    return { sessionId: this.#session, target: { surface: "panel", revision },
      get model() {
        const value = lease.value
        if (value.kind !== "ui_panels") throw new Error("panel lease is unavailable")
        return value.panels[index]!.model
      }, release: () => lease.release() }
  }
  #key(kind: "catalog" | "panels"): string { return `ui:${this.#generation}:${this.#session}:${kind}` }
  async #read(kind: "catalog" | "panels"): Promise<void> {
    const session = this.#session
    if (session === null) return
    const reservation = this.#cache.reserve((kind === "catalog" ? MAX_UI_DESCRIPTOR_BYTES : MAX_UI_PANELS_BYTES) * UI_COLLECTION_ALLOCATION_FACTOR)
    if (reservation === null) { this.#error = "UI content cache is full with active readers."; this.#changed(); return }
    const request = new AbortController(), generation = this.#generation
    this.#requests.set(kind, request)
    this.#changed()
    let previous: CacheLease<HistoryCacheValue> | null = null
    try {
      let value: HistoryCacheValue
      if (kind === "catalog") {
        const catalog = await this.#reader.uiCatalog(session, request.signal)
        if (generation !== this.#generation || request.signal.aborted) return
        const identities = new Set(catalog.entries.map(uiIdentity))
        if (identities.size !== catalog.entries.length) throw new Error("duplicate contribution identity")
        value = { kind: "ui_catalog", catalog }
      } else {
        const panels = await this.#reader.uiPanels(session, request.signal)
        if (generation !== this.#generation || request.signal.aborted) return
        const current = this.snapshot.panels
        if (current.length === panels.panels.length && current.every((panel, index) => {
          const next = panels.panels[index]!
          return panel.revision === next.revision && uiIdentity(panel.model.presentation) === uiIdentity(next.presentation)
        })) return
        value = { kind: "ui_panels", panels: prepareUiPanels(panels) }
      }
      const lease = reservation.commit(this.#key(kind), value)
      if (kind === "catalog") { previous = this.#catalog; this.#catalog = lease }
      else { previous = this.#panels; this.#panels = lease }
    } catch {
      if (generation === this.#generation && !request.signal.aborted) this.#error = "UI refresh failed. Close and reopen to retry."
    } finally {
      reservation.release()
      if (generation === this.#generation) {
        this.#requests.delete(kind)
        try { this.#changed() } finally { previous?.release() }
      } else previous?.release()
    }
  }
}
