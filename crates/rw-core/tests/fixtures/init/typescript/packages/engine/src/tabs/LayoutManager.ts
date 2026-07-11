import type { BaseWindow, WebContentsView } from 'electron'
import type { Insets } from '@shared/types'

/**
 * Owns which tab views are attached to the window and where they sit.
 * The content region is the window minus the chrome insets the UI reports.
 * Supports 1 view (single) or 2 views (split, side by side).
 */
export class LayoutManager {
  private insets: Insets = { top: 96, left: 0, right: 0, bottom: 0 }
  private attached: WebContentsView[] = []

  constructor(private readonly win: BaseWindow) {}

  setInsets(insets: Insets): void {
    this.insets = insets
  }

  /** Attach exactly `views` (0..2) and lay them out; detach anything else. */
  apply(views: WebContentsView[]): void {
    for (const v of this.attached) {
      if (!views.includes(v)) this.win.contentView.removeChildView(v)
    }
    for (const v of views) {
      if (!this.attached.includes(v)) this.win.contentView.addChildView(v)
    }
    this.attached = [...views]
    this.position()
  }

  position(): void {
    const b = this.win.getContentBounds()
    const x = this.insets.left
    const y = this.insets.top
    const w = Math.max(0, b.width - this.insets.left - this.insets.right)
    const h = Math.max(0, b.height - this.insets.top - this.insets.bottom)

    if (this.attached.length === 1) {
      this.attached[0].setBounds({ x, y, width: w, height: h })
    } else if (this.attached.length === 2) {
      const gap = 1
      const half = Math.floor((w - gap) / 2)
      this.attached[0].setBounds({ x, y, width: half, height: h })
      this.attached[1].setBounds({ x: x + half + gap, y, width: w - half - gap, height: h })
    }
  }
}
