import { DiffRenderable, ScrollBoxRenderable, TextBufferRenderable, type RenderContext, type Renderable } from "@opentui/core"

/** A diff has percentage-sized native children; its scroll owner supplies their actual line extent. */
export class ReviewDiffViewport extends ScrollBoxRenderable {
  constructor(ctx: RenderContext, readonly diff: DiffRenderable) {
    super(ctx, { id: "session-review-viewport", width: "100%", height: 1, scrollY: true,
      scrollX: false, viewportCulling: false, contentOptions: { width: "100%" } })
    this.add(diff)
  }
  protected override onUpdate(deltaTime: number): void {
    // OpenTUI's fixed diff tree has at most two code sides and their gutters.
    // Read their native line counts, never reparse or split the source per frame.
    this.content.height = Math.max(this.viewport.height, nativeRows(this.diff, 0))
    this.diff.height = Math.max(1, this.viewport.height)
    this.diff.translateY = this.scrollTop
    scrollNative(this.diff, this.scrollTop, 0)
    super.onUpdate(deltaTime)
  }
}
function nativeRows(node: Renderable, depth: number): number {
  if (node instanceof TextBufferRenderable) return Math.max(1, node.lineCount)
  if (depth === 3) return 1
  let rows = 1
  for (const child of node.getChildren()) rows = Math.max(rows, nativeRows(child, depth + 1))
  return rows
}

function scrollNative(node: Renderable, offset: number, depth: number): void {
  if (node instanceof TextBufferRenderable) { node.scrollY = offset; return }
  if (depth === 3) return
  for (const child of node.getChildren()) scrollNative(child, offset, depth + 1)
}
