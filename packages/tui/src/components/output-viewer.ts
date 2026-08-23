import {
  BoxRenderable,
  ScrollBoxRenderable,
  TextRenderable,
  type RenderContext,
} from "@opentui/core"

import { getScrollAcceleration, presentTool } from "../render"
import type { ToolProjection } from "../state"
import type { RottweilerTheme } from "../theme"
import { toolDisplayName, toolOutputContent } from "./transcript"

/** Read-only, full-height presentation for one tool's complete output. */
export class OutputViewerRenderable extends BoxRenderable {
  readonly header: TextRenderable
  readonly scroller: ScrollBoxRenderable
  readonly body: TextRenderable
  readonly hint: TextRenderable
  #toolCallId: string | null = null

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
    this.scroller.add(this.body)
    this.add(this.header)
    this.add(this.scroller)
    this.add(this.hint)
    this.resizeForTerminal(ctx.height)
  }

  get toolCallId(): string | null {
    return this.#toolCallId
  }

  /** Open at the beginning of the complete output. */
  open(tool: ToolProjection): void {
    this.#toolCallId = tool.toolCallId
    this.update(tool)
    this.visible = true
    this.scroller.scrollTo(0)
    this.scroller.focus()
  }

  /** Refresh content without disturbing the reader's current scroll position. */
  update(tool: ToolProjection): void {
    if (this.#toolCallId !== tool.toolCallId) return
    const subject = presentTool(tool).subject.replace(/\s+/g, " ").trim()
    this.header.content = `${toolDisplayName(tool.name)} · ${subject}`
    this.body.content = toolOutputContent(tool)
  }

  closePresentation(): void {
    this.#toolCallId = null
    this.scroller.blur()
    this.visible = false
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
    this.hint.height = contentRows >= 2 ? 1 : 0
    this.hint.visible = contentRows >= 2
    this.scroller.height = Math.max(1, contentRows - this.header.height - this.hint.height)
  }
}
