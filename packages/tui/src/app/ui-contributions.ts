import type { OutputViewerRenderable } from "../components/output-viewer"
import type { ClientCache } from "../history/cache"
import type { HistoryCacheValue } from "../history/controller"
import type { DocumentController, DocumentSnapshot } from "../history/document"
import type { PickerController } from "../picker-controller"
import type { ProjectionRequestBroker } from "../projection-requests"
import type { SessionReader } from "../session-reader"
import { UiActionController } from "../ui/actions"
import { UiCatalogController } from "../ui/catalog"
import { boundedUiText } from "../ui-presentation"
import { uiIdentity } from "../ui/presentation"

interface ContributionHost {
  readonly sessionId: string
  readonly writable: boolean
  readonly destroyed: boolean
  readonly viewer: OutputViewerRenderable
  readonly document: DocumentController
  readonly picker: PickerController
  readonly requests: ProjectionRequestBroker
  closePicker(): void
  refresh(): void
}

/** Native contribution interaction owner. Plugin execution remains an engine command. */
export class UiContributionController {
  readonly #host: ContributionHost
  readonly #catalog: UiCatalogController
  readonly #actions: UiActionController
  #mode: "closed" | "tool" | "panels" = "closed"
  #selected: string | null = null
  #error: string | null = null
  constructor(host: ContributionHost, reader: SessionReader, cache: ClientCache<HistoryCacheValue>) {
    this.#host = host
    this.#catalog = new UiCatalogController(reader, cache, () => this.rebind())
    this.#actions = new UiActionController({
      allowed: lease => host.writable && lease.sessionId === host.sessionId && this.#catalog.current(lease.model.presentation),
      execute: (session, request) => host.requests.emit({ type: "invoke_ui_action", meta: host.requests.meta(), session_id: session, request }),
      changed: () => this.rebind(), failed: message => { this.#error = boundedUiText(message, 256); this.rebind() },
    })
  }
  get pending(): boolean { return this.#actions.pending }
  openPanels(): void {
    this.close()
    this.#mode = "panels"
    this.#host.document.close()
    this.#host.picker.begin("uiPanels")
    this.#catalog.open(this.#host.sessionId, "panels")
    this.renderPicker()
  }
  documentChanged(snapshot: DocumentSnapshot): void {
    if (this.#mode === "panels") return
    if (!snapshot.open) { this.close(); return }
    if (snapshot.surface === null) return
    if (this.#mode !== "tool") {
      this.#mode = "tool"
      if (this.#host.writable && snapshot.surface.presentation.descriptor.actions.length > 0) {
        this.#catalog.open(this.#host.sessionId, "catalog")
      }
    }
    this.rebind()
  }
  pickerClosed(): void {
    if (this.#mode === "panels" && this.#selected === null) this.close()
  }
  close(): void {
    if (this.#mode === "panels") this.#host.viewer.closePresentation()
    else this.#host.viewer.setActions(null, false, null)
    this.#mode = "closed"; this.#selected = null; this.#error = null
    this.#actions.reset()
    if (this.#host.picker.kind === "uiPanels") this.#host.closePicker()
    this.#catalog.close()
  }
  renderPicker(): void {
    if (this.#host.picker.kind !== "uiPanels") return
    const snapshot = this.#catalog.snapshot
    const panels = snapshot.entries.filter(entry => entry.descriptor.surface.surface === "panel")
    if (snapshot.loading && panels.length === 0) {
      this.#host.picker.showLoading("Extension panels", "Loading approved panels")
    } else if (snapshot.error !== null) {
      this.#host.picker.show("Extension panels", [{ id: "retry", label: "Retry loading panels", description: snapshot.error, value: null }], () => this.#catalog.refresh())
    } else if (panels.length === 0) {
      this.#host.picker.showStatus("Extension panels", "No panels", "Approved extensions can provide declarative panels.")
    } else {
      this.#host.picker.show("Extension panels", panels.map(entry => ({
        id: uiIdentity(entry), label: entry.descriptor.title, description: entry.owner.extension, value: uiIdentity(entry),
      })), item => {
        this.#selected = item.value
        this.#host.closePicker()
        this.rebind()
        this.#host.refresh()
        this.#host.viewer.scroller.scrollTo(0)
        this.#host.viewer.focusPresentation()
      })
    }
  }
  rebind(): void {
    if (this.#host.destroyed) return
    if (this.#mode === "closed") {
      this.#host.viewer.setActions(null, false, null)
      return
    }
    const catalog = this.#catalog.snapshot
    if (this.#mode === "panels") {
      if (this.#host.picker.kind === "uiPanels") this.renderPicker()
      if (this.#selected === null) return
      const panel = catalog.panels.find(panel => uiIdentity(panel.model.presentation) === this.#selected)
      const declared = catalog.entries.some(entry => uiIdentity(entry) === this.#selected)
      const surface = declared ? panel?.model ?? null : null
      this.#host.viewer.showDocument({ open: true, page: null, surface,
        loading: catalog.loading, previous: false, error: surface === null ? "Panel data is unavailable." : null,
      })
      if (surface === null) { this.#host.viewer.setActions(null, false, null); return }
    }
    const surface = this.#mode === "panels"
      ? catalog.panels.find(panel => uiIdentity(panel.model.presentation) === this.#selected)?.model
      : this.#host.document.snapshot.surface
    if (surface == null) return
    const enabled = this.#host.writable && !this.#actions.pending && this.#catalog.current(surface.presentation)
    this.#host.viewer.setActions(surface.presentation.descriptor.actions, enabled, id => this.#invoke(id))
    if (surface.presentation.descriptor.actions.length > 0) {
      this.#host.viewer.hint.content = this.#error ?? catalog.error ?? (this.#actions.pending ? "Action pending · Esc close"
        : enabled ? "Tab actions · Enter run · Esc close" : "Actions unavailable for this extension generation · Esc close")
    }
  }
  #invoke(id: string): void {
    const lease = this.#mode === "panels" && this.#selected !== null
      ? this.#catalog.pinPanel(this.#selected) : this.#host.document.pinAction()
    if (lease === null) return
    this.#error = null
    void this.#actions.invoke(lease, id)
  }
}
