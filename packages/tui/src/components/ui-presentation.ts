import { BoxRenderable, TextRenderable, type RenderContext } from "@opentui/core"
import type { RottweilerTheme } from "../theme"
import type { UiSurfaceModel } from "../ui/presentation"

/** Native field nodes retain strings from the charged source model; no plugin code runs here. */
export class UiPresentationRenderable extends BoxRenderable {
  readonly #context: RenderContext
  readonly #theme: RottweilerTheme
  #model: UiSurfaceModel | null = null

  constructor(ctx: RenderContext, theme: RottweilerTheme) {
    super(ctx, { width: "100%", flexDirection: "column", flexShrink: 0, visible: false })
    this.#context = ctx
    this.#theme = theme
  }

  setSurface(model: UiSurfaceModel | null): void {
    if (model === this.#model) return
    this.#model = model
    for (const child of this.getChildren()) {
      if (child instanceof TextRenderable) child.content = ""
      child.destroyRecursively()
    }
    this.visible = model !== null
    if (model === null) return
    for (const field of model.fields) {
      this.add(new TextRenderable(this.#context, {
        id: `ui-field:${field.id}`, content: field.text,
        fg: field.kind === "badge" ? this.#theme.accent : this.#theme.text,
        width: "100%", flexShrink: 0, marginBottom: 1, selectable: true,
      }))
    }
  }
  override destroyRecursively(): void {
    this.setSurface(null)
    super.destroyRecursively()
  }

  override destroy(): void {
    this.setSurface(null)
    super.destroy()
  }

}
