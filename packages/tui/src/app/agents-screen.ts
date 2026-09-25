import type { ListDetailRenderable, PickerItem } from "../components"
import {
  agentsBrowserChildren,
  childAgentName,
  childRunning,
  childStatus,
  childTask,
  createAgentsBrowserModel,
  HIDE_FINISHED_KEY,
  type AgentsBrowserAction,
  type AgentsBrowserChild,
} from "../agents-browser"
import type { PickerCloseReason, PickerKind } from "../picker-controller"
import type { RottweilerState } from "../state"
import { boundedUiText } from "../ui-presentation"
import { keyStrokeFromEvent } from "../keybindings"
import type { ChildUiController } from "./children"
import { presentScreen, resizeScreen, screenQuery, type ScreenShell } from "./screen-shell"

interface AgentsScreenHost {
  readonly state: RottweilerState
  readonly browser: ListDetailRenderable<AgentsBrowserAction>
  readonly children: ChildUiController
  projectError(code: string, message: string, retryable?: boolean): void
}

type AgentAction = "view" | "message" | "stop" | "background" | "close"

const MAX_AGENT_MESSAGE_BYTES = 8192

/**
 * Agents screen: every child of the session in the shared list-detail
 * anatomy. Enter opens one child's actions; each action is explicit, and
 * none of them touches the parent's composer or stops the parent's turn.
 */
export class AgentsScreenController {
  readonly #host: AgentsScreenHost
  readonly #shell: ScreenShell
  #actionsFor: string | null = null
  #prompting = false
  #returnToList = true

  constructor(host: AgentsScreenHost, shell: ScreenShell) {
    this.#host = host
    this.#shell = shell
  }

  open(): void {
    if (this.#host.state.replay.active) {
      this.#host.projectError(
        "subagents_unavailable_in_replay",
        "Child-agent controls are available from the live parent session, not historical replay.",
      )
      return
    }
    this.#shell.pickerController.begin("agents")
    this.resize(this.#shell.terminalWidth, this.#shell.terminalHeight)
    this.#host.children.requestSubagents()
    this.#shell.pickerController.refresh()
    this.#host.browser.input.focus()
  }

  resize(width: number, height: number): void {
    resizeScreen(this.#shell, this.#host.browser, width, height)
  }

  /** Whether closing `kind` should reveal the agents list again. */
  restoresList(kind: PickerKind | null, reason: PickerCloseReason): boolean {
    const restore = reason === "dismiss" && kind === "agentActions" && this.#returnToList
    this.#actionsFor = null
    this.#prompting = false
    this.#returnToList = true
    return restore
  }

  /** Ctrl+B: detach the child a foreground spawn or wait is blocked on. */
  backgroundForeground(): boolean {
    const id = this.#host.children.foregroundId
    if (id === null) return false
    void this.#host.children.backgroundSubagent(id)
    return true
  }

  render(kind: "agents" | "agentActions"): void {
    if (kind === "agents") this.#renderList()
    else if (!this.#prompting) this.#renderActions()
  }

  #renderList(): void {
    const browser = this.#host.browser
    const children = this.#host.children
    const query = screenQuery(this.#shell, browser)
    const model = createAgentsBrowserModel({
      state: this.#host.state,
      catalog: children.catalog,
      pending: children.familyPending,
      foreground: children.foregroundId,
      finishedInStrip: children.finishedInStrip,
      loading: children.listLoading,
      error: children.listError,
      query,
      selectedId: browser.visible ? browser.selectedId : null,
    })
    presentScreen(this.#shell, browser, model, action => this.#activate(action), key => {
      if (keyStrokeFromEvent(key) !== HIDE_FINISHED_KEY || children.finishedInStrip === 0) return false
      children.hideFinished()
      this.#shell.pickerController.refresh()
      return true
    })
  }

  #activate(action: AgentsBrowserAction): void {
    switch (action.kind) {
      case "retry":
        this.#host.children.retryListing()
        this.#shell.pickerController.refresh()
        return
      case "control":
        this.#returnToList = false
        this.#shell.closePicker()
        this.#host.children.enterFamily(action.row)
        return
      case "child":
        this.#actionsFor = action.subagentId
        this.#host.browser.visible = false
        this.#host.browser.input.blur()
        this.#shell.pickerController.kind = "agentActions"
        this.#shell.pickerController.refresh()
    }
  }

  #child(): AgentsBrowserChild | undefined {
    const id = this.#actionsFor
    return id === null ? undefined : agentsBrowserChildren(this.#host.state, this.#host.children.catalog)
      .find(child => child.subagentId === id)
  }

  #renderActions(): void {
    const child = this.#child()
    if (child === undefined) { this.#shell.closePicker(); return }
    const running = childRunning(child)
    const inCatalog = child.descriptor !== null
    const foreground = this.#host.children.foregroundId === child.subagentId
    const items: PickerItem<AgentAction>[] = [
      { id: "view", label: "View transcript", description: "Watch this agent; Esc returns to the parent, which keeps running", value: "view" },
      ...(foreground ? [{ id: "background", label: "Move to background", description: "The parent stops waiting and continues; the result arrives when the agent finishes", value: "background" as const }] : []),
      ...(running
        ? [{ id: "stop", label: "Stop", description: "Interrupt this agent; its partial report is delivered to the parent", value: "stop" as const }]
        : inCatalog ? [{ id: "message", label: "Message", description: "Send a follow-up; the agent keeps the context of its earlier work", value: "message" as const }] : []),
      ...(inCatalog ? [{ id: "close", label: "Close", description: "Release this agent and its worktree; its result stays in the parent transcript", value: "close" as const }] : []),
    ]
    const title = `${childAgentName(child)} · ${childStatus(child)} · ${boundedUiText(childTask(child), 64)}`
    this.#shell.pickerController.show(title, items, item => this.#run(child, item.value), { back: () => this.#backToList() })
  }

  /** Esc on a child's actions returns to the agents list. */
  #backToList(): void {
    this.#actionsFor = null
    this.#prompting = false
    this.open()
  }

  /** Esc in the message prompt returns to the child's actions. */
  #backToActions(): void {
    this.#prompting = false
    this.#shell.pickerController.kind = "agentActions"
    this.#shell.pickerController.refresh()
  }

  /**
   * The engine accepts follow-ups only for idle children. A child can start
   * again between opening the prompt and sending it, so this refuses with a
   * hint and keeps the typed text out of the engine's error path.
   */
  #refuseWhileRunning(): boolean {
    const current = this.#child()
    if (current === undefined || !childRunning(current)) return false
    this.#host.projectError(
      "subagent_still_running",
      "This agent is still working · wait for it to finish or stop it, then send the message",
    )
    return true
  }

  #run(child: AgentsBrowserChild, action: AgentAction): void {
    const children = this.#host.children
    switch (action) {
      case "view":
        this.#returnToList = false
        this.#shell.closePicker()
        void children.enterSubagent(child.subagentId)
        return
      case "message":
        if (this.#refuseWhileRunning()) { this.#shell.pickerController.refresh(); return }
        this.#prompting = true
        this.#shell.pickerController.openTextPrompt({
          title: `Message ${childAgentName(child)}`,
          placeholder: "Follow-up for this agent",
          maxBytes: MAX_AGENT_MESSAGE_BYTES,
          empty: "reject",
          onSubmit: content => {
            if (this.#refuseWhileRunning()) return
            this.#shell.closePicker()
            void children.messageSubagent(child.subagentId, content)
          },
        }, () => this.#backToActions())
        return
      case "stop":
        this.#shell.closePicker()
        void children.interruptSubagent(child.subagentId)
        return
      case "background":
        this.#shell.closePicker()
        void children.backgroundSubagent(child.subagentId)
        return
      case "close":
        this.#shell.closePicker()
        void children.closeSubagent(child.subagentId)
    }
  }
}
