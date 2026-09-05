import { UiPresentationRenderable } from "./ui-presentation"
import type { DocumentSnapshot } from "../history/document"
import {
  BoxRenderable,
  ScrollBoxRenderable,
  TextRenderable,
  type RenderContext,
} from "@opentui/core"

import { getScrollAcceleration, presentTool } from "../render"
import type { ToolProjection } from "../state"
import type { RottweilerTheme } from "../theme"
import { toolDisplayName } from "./transcript"
import { toolOutputContent } from "./transcript/blocks"

/** Read-only, full-height presentation for one tool's complete output. */
export class OutputViewerRenderable extends BoxRenderable {
  readonly header: TextRenderable
  readonly scroller: ScrollBoxRenderable
  readonly body: TextRenderable
  readonly surface: UiPresentationRenderable
  readonly hint: TextRenderable
  #toolCallId: string | null = null
  #documentPage: DocumentSnapshot["page"] = null

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
    this.surface = new UiPresentationRenderable(ctx, theme)
    this.scroller.add(this.body)
    this.scroller.add(this.surface)
    this.add(this.header)
    this.add(this.scroller)
    this.add(this.hint)
    this.resizeForTerminal(ctx.height)
  }

  get toolCallId(): string | null {
    return this.#toolCallId
  }

  showDocument(snapshot: DocumentSnapshot): void {
    if (!snapshot.open) return
    const changed = this.#documentPage !== snapshot.page
    this.#documentPage = snapshot.page
    this.#toolCallId = null
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
    this.body.visible = true
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
    this.#documentPage = null
    this.scroller.blur()
    this.visible = false
    this.body.content = ""
    this.surface.setSurface(null)
    this.header.content = ""
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
