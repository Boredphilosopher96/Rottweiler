import type { RenderContext, TextRenderable } from "@opentui/core"

/** Keep click actions distinct from text-selection drags and repeated-click selection. */
export function bindSelectableClick(
  ctx: RenderContext,
  target: TextRenderable,
  action: () => void,
): void {
  let pressed = false
  target.onMouseDown = (event) => { pressed = event.button === 0 }
  target.onMouseDrag = () => { pressed = false }
  target.onMouseOut = () => { pressed = false }
  target.onMouseUp = (event) => {
    const clicked = pressed && event.button === 0
    pressed = false
    if (!clicked) return
    const selection = ctx.getSelection()
    if (selection !== null && !selection.isStart) return
    ctx.clearSelection()
    action()
  }
}
