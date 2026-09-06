import { TextRenderable as NativeTextRenderable, type StyledText, type RenderContext, type TextOptions } from "@opentui/core"

/** Display replacement owns no edit history, including when its new text is empty. */
export class TextRenderable extends NativeTextRenderable {
  constructor(ctx: RenderContext, options: TextOptions) {
    super(ctx, options)
    const updateNodes = this.onLifecyclePass
    this.onLifecyclePass = () => {
      const changed = this.rootTextNode.isDirty
      updateNodes()
      if (changed) this.#resetEmptyBuffer()
    }
  }
  override get content(): StyledText { return super.content }
  override set content(value: StyledText | string) {
    super.content = value
    this.#resetEmptyBuffer()
  }
  override clear(): void { super.clear(); this.#resetEmptyBuffer() }
  #resetEmptyBuffer(): void {
    // Native clear preserves rope arena allocations for editable history. A
    // display label instead replaces its content, so return that empty arena to
    // its reusable capacity just as nonempty styled replacement does.
    if (this.textBuffer.byteSize === 0) this.textBuffer.reset()
  }
}
