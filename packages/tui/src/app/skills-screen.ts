import type { ComposerRenderable, ListDetailRenderable } from "../components"
import type { ProjectionRequestBroker } from "../projection-requests"
import type { EngineEvent } from "../protocol"
import { presentError } from "../render"
import { createSkillsBrowserModel, type SkillsBrowserAction, type SkillsInventory } from "../skills-browser"
import { presentScreen, resizeScreen, screenQuery, type ScreenShell } from "./screen-shell"

interface SkillsScreenHost {
  readonly sessionId: string
  readonly replay: boolean
  readonly browser: ListDetailRenderable<SkillsBrowserAction>
  readonly composer: ComposerRenderable
  readonly requests: ProjectionRequestBroker
  focusComposer(): void
}

/**
 * Skills screen: the engine's declarative extension inventory. Loaded skills
 * and commands insert their slash name into the composer; everything else
 * explains why it did not load and how to fix it.
 */
export class SkillsScreenController {
  readonly #host: SkillsScreenHost
  readonly #shell: ScreenShell
  #inventory: SkillsInventory = { kind: "loading" }
  #requestId: string | null = null

  constructor(host: SkillsScreenHost, shell: ScreenShell) {
    this.#host = host
    this.#shell = shell
  }

  open(): void {
    this.#shell.pickerController.begin("skills")
    this.resize(this.#shell.terminalWidth, this.#shell.terminalHeight)
    this.#request()
    this.#shell.pickerController.refresh()
    this.#host.browser.input.focus()
  }

  resize(width: number, height: number): void {
    resizeScreen(this.#shell, this.#host.browser, width, height)
  }

  /** Accepts the inventory answering this screen's latest request. */
  accept(event: Extract<EngineEvent, { type: "extensions_listed" }>): void {
    if (event.session_id !== this.#host.sessionId || event.meta.request_id !== this.#requestId) return
    this.#requestId = null
    this.#inventory = { kind: "ready", entries: event.entries, truncated: event.truncated }
    if (this.#shell.pickerController.kind === "skills") this.#shell.pickerController.refresh()
  }

  reset(): void {
    this.#requestId = null
    this.#inventory = { kind: "loading" }
  }

  #request(): void {
    if (this.#host.replay) {
      this.#inventory = { kind: "error", message: "Skills are listed from the live session, not historical replay." }
      return
    }
    const meta = this.#host.requests.meta()
    this.#requestId = meta.request_id
    if (this.#inventory.kind === "error") this.#inventory = { kind: "loading" }
    void this.#host.requests.consume({ type: "list_extensions", meta, session_id: this.#host.sessionId }, outcome => {
      if (this.#requestId !== meta.request_id) return
      if (outcome?.type === "rejected") this.#fail(outcome.error.category, outcome.error.code, outcome.error.message)
      else if (outcome == null) this.#fail("protocol", "extensions_unavailable", "Couldn't load skills because the engine connection is unavailable.")
    }).catch(error => {
      if (this.#requestId === meta.request_id) {
        this.#fail("protocol", "extensions_failed", error instanceof Error ? error.message : "the request could not be delivered to the engine")
      }
    })
  }

  #fail(category: string, code: string, message: string): void {
    this.#requestId = null
    this.#inventory = { kind: "error", message: presentError({ category, code, message }).text }
    if (this.#shell.pickerController.kind === "skills") this.#shell.pickerController.refresh()
  }

  render(): void {
    const browser = this.#host.browser
    const query = screenQuery(this.#shell, browser)
    const model = createSkillsBrowserModel({
      inventory: this.#inventory,
      query,
      selectedId: browser.visible ? browser.selectedId : null,
    })
    presentScreen(this.#shell, browser, model, action => this.#activate(action))
  }

  #activate(action: SkillsBrowserAction): void {
    switch (action.kind) {
      case "inspect": return
      case "retry":
        this.#request()
        this.#shell.pickerController.refresh()
        return
      case "insert": {
        this.#shell.closePicker()
        const composer = this.#host.composer
        const draft = composer.value
        composer.value = `/${action.name} ${draft.replace(/^\/\S*\s?/u, "")}`
        this.#host.focusComposer()
      }
    }
  }
}
