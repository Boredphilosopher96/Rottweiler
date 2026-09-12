import { TextRenderable } from "./text"
import { ToolOutputReader } from "../state/output-reader"
import { UiActionsRenderable } from "./ui-actions"
import type { UiPresentation } from "../protocol"
import type { KeyEvent } from "@opentui/core"
import { UiPresentationRenderable } from "./ui-presentation"
import type { DocumentSnapshot } from "../history/document"
import {
  BoxRenderable,
  ScrollBoxRenderable,
  type RenderContext,
} from "@opentui/core"

import { getScrollAcceleration, presentTool } from "../render"
import type { ToolProjection } from "../state"
import type { RottweilerTheme } from "../theme"
import { toolDisplayName } from "./transcript"
import { toolOutputContent } from "./transcript/blocks"

/** Source-backed document or declarative surface with engine-mediated actions. */
export class OutputViewerRenderable extends BoxRenderable {
  readonly header: TextRenderable
  readonly scroller: ScrollBoxRenderable
  readonly body: TextRenderable
  readonly surface: UiPresentationRenderable
  readonly hint: TextRenderable
  readonly actions: UiActionsRenderable
  #invocationId: string | null = null
  readonly #liveOutput = new ToolOutputReader()
  #documentPage: DocumentSnapshot["page"] = null

  override destroy(): void {
    this.#liveOutput.clear()
    this.#documentPage = null; this.#invocationId = null
    super.destroy()
  }

  constructor(ctx: RenderContext, theme: RottweilerTheme) {
    super(ctx, {
      id: "tool-output-viewer",
      width: "100%",
      height: 17,
      flexShrink: 0,
      flexDirection: "column",
      border: true,
      borderStyle: "rounded",
      borderColor: theme.info,
      backgroundColor: theme.backgroundPanel,
      paddingX: 1,
      visible: false,
      zIndex: 9,
    })
    this.header = new TextRenderable(ctx, {
      content: "",
      fg: theme.text,
      height: 1,
      flexShrink: 0,
      wrapMode: "none",
      truncate: true,
      selectable: true,
    })
    this.scroller = new ScrollBoxRenderable(ctx, {
      id: "tool-output-scroll",
      width: "100%",
      height: 13,
      scrollY: true,
      scrollX: false,
      scrollAcceleration: getScrollAcceleration(),
      viewportCulling: true,
      contentOptions: { flexDirection: "column", width: "100%" },
      verticalScrollbarOptions: {
        showArrows: false,
        trackOptions: { backgroundColor: theme.backgroundPanel },
      },
    })
    this.body = new TextRenderable(ctx, {
      id: "tool-output-content",
      content: "",
      fg: theme.text,
      width: "100%",
      flexShrink: 0,
      wrapMode: "word",
      selectable: true,
    })
    this.hint = new TextRenderable(ctx, {
      content: "Esc to close",
      fg: theme.textMuted,
      height: 1,
      flexShrink: 0,
    })
    this.actions = new UiActionsRenderable(ctx, theme)
    this.surface = new UiPresentationRenderable(ctx, theme)
    this.scroller.add(this.body)
    this.scroller.add(this.surface)
    this.add(this.header)
    this.add(this.scroller)
    this.add(this.actions)
    this.add(this.hint)
    this.resizeForTerminal(ctx.height)
  }

  get invocationId(): string | null {
    return this.#invocationId
  }

  showDocument(snapshot: DocumentSnapshot): void {
    if (!snapshot.open) return
    this.#liveOutput.clear()
    const changed = this.#documentPage !== snapshot.page
    this.#documentPage = snapshot.page
    this.#invocationId = null
    this.visible = true
    const page = snapshot.page
    this.surface.setSurface(snapshot.surface)
    this.body.visible = snapshot.surface === null
    this.header.content = page === null ? "Content" : `Content · bytes ${page.offset + 1}–${page.next_offset ?? page.total_bytes} of ${page.total_bytes}`
    if (snapshot.surface !== null) this.header.content = snapshot.surface.presentation.descriptor.title
    if (changed) this.body.content = page?.text ?? ""
    this.hint.content = snapshot.error ?? (snapshot.loading ? "Loading content…"
      : `${snapshot.previous ? "← previous · " : ""}${page?.next_offset != null ? "next → · " : ""}Esc to close`)
    if (changed) this.scroller.scrollTo(0)
  }

  /** Open at the beginning of the complete output. */
  open(tool: ToolProjection): void {
    this.#documentPage = null
    this.surface.setSurface(null)
    this.setActions(null, false, null)
    this.body.visible = true
    this.#liveOutput.clear()
    this.#invocationId = tool.invocationId
    this.update(tool)
    this.visible = true
    this.scroller.scrollTo(0)
    this.scroller.focus()
  }

  /** Refresh content without disturbing the reader's current scroll position. */
  update(tool: ToolProjection): void {
    if (this.#invocationId !== tool.invocationId) return
    const subject = presentTool(tool).subject.replace(/\s+/g, " ").trim()
    this.header.content = `${toolDisplayName(tool.name)} · ${subject}`
    if (tool.status === "finished") this.#liveOutput.clear()
    this.body.content = toolOutputContent(tool, tool.status === "finished" ? null : this.#liveOutput.read(tool.chunks))
  }

  closePresentation(): void {
    this.#liveOutput.clear()
    this.#invocationId = null
    this.#documentPage = null
    this.scroller.blur()
    this.visible = false
    this.body.content = ""
    this.surface.setSurface(null)
    this.setActions(null, false, null)
    this.header.content = ""
  }

  setActions(actions: UiPresentation["descriptor"]["actions"] | null, enabled: boolean, activate: ((id: string) => void) | null): void {
    this.actions.update(actions, enabled, activate)
    this.resizeForTerminal(this.ctx.height)
  }

  handleKey(key: KeyEvent): boolean {
    if (!this.visible) return false
    if (key.name === "tab" && !key.ctrl && !key.meta && this.actions.visible) {
      if (this.actions.focused) this.scroller.focus()
      else this.actions.focus()
      return true
    }
    return this.actions.handleKey(key)
  }

  captureInteraction(): { scrollTop: number; action: unknown; actionsFocused: boolean } | null {
    return this.visible ? { scrollTop: this.scroller.scrollTop, action: this.actions.getSelectedOption()?.value,
      actionsFocused: this.actions.focused } : null
  }

  restoreInteraction(state: NonNullable<ReturnType<OutputViewerRenderable["captureInteraction"]>>): void {
    if (!this.visible) return
    const index = this.actions.options.findIndex(action => action.value === state.action)
    if (index >= 0) this.actions.setSelectedIndex(index)
    this.scroller.scrollTo(state.scrollTop)
    if (state.actionsFocused && this.actions.visible) this.actions.focus()
    else this.scroller.focus()
  }

  focusPresentation(): void {
    this.scroller.focus()
  }

  /** Keep the overlay and its scroll viewport inside the terminal. */
  resizeForTerminal(terminalHeight: number): void {
    const panelHeight = Math.max(4, terminalHeight - 2)
    this.height = panelHeight
    const contentRows = Math.max(1, panelHeight - 2)
    this.header.height = 1
    this.header.visible = true
    const actionCount = this.actions.options.length
    const hintRows = contentRows >= (actionCount > 0 ? 4 : 2) ? 1 : 0
    this.hint.height = hintRows
    this.hint.visible = hintRows > 0
    const available = contentRows - 1 - hintRows
    const actionRows = Math.min(actionCount, Math.max(0, available - (available > 1 ? 1 : 0)))
    this.actions.height = actionRows
    this.actions.visible = actionRows > 0
    this.scroller.height = Math.max(actionCount > 0 ? 0 : 1, available - actionRows)
    this.scroller.visible = actionCount === 0 || available > actionRows
  }
}
