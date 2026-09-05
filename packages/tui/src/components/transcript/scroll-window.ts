import { ScrollBoxRenderable, type RenderContext, type ScrollBoxOptions } from "@opentui/core"

/** A bounded physical window; the parent owns logical history and stable anchors. */
export class TranscriptScrollWindow extends ScrollBoxRenderable {
  afterLayout: (() => void) | null = null

  constructor(ctx: RenderContext, options: ScrollBoxOptions) { super(ctx, options) }

  override updateLayout(...args: Parameters<ScrollBoxRenderable["updateLayout"]>): void {
    super.updateLayout(...args)
    this.afterLayout?.()
  }
}
