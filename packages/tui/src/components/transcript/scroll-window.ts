import type { ClientDiagnostics } from "../../client-diagnostics"
import { ScrollBoxRenderable, type RenderContext, type ScrollBoxOptions } from "@opentui/core"

/** A bounded physical window; the parent owns logical history and stable anchors. */
export class TranscriptScrollWindow extends ScrollBoxRenderable {
  diagnostics: ClientDiagnostics | undefined
  afterLayout: (() => void) | null = null

  constructor(ctx: RenderContext, options: ScrollBoxOptions) { super(ctx, options) }

  override updateLayout(...args: Parameters<ScrollBoxRenderable["updateLayout"]>): void {
    const startedAt = this.diagnostics?.start()
    try {
      super.updateLayout(...args)
      this.afterLayout?.()

    } finally { if (startedAt !== undefined) this.diagnostics?.finish("history_layout", startedAt) }
  }
}
