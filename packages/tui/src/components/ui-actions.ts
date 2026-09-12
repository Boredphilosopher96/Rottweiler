import { SelectRenderable, SelectRenderableEvents, type KeyEvent, type RenderContext } from "@opentui/core"
import type { UiPresentation } from "../protocol"
import type { RottweilerTheme } from "../theme"

/** A small native action list; its labels are borrowed from a pinned surface. */
export class UiActionsRenderable extends SelectRenderable {
  #actions: UiPresentation["descriptor"]["actions"] | null = null
  #enabled = false
  #activate: ((id: string) => void) | null = null
  readonly #theme: RottweilerTheme
  constructor(ctx: RenderContext, theme: RottweilerTheme) {
    super(ctx, { width: "100%", height: 0, visible: false, options: [], flexShrink: 0,
      backgroundColor: theme.backgroundPanel, textColor: theme.accent,
      selectedBackgroundColor: theme.backgroundElement, selectedTextColor: theme.text,
      showDescription: false, showScrollIndicator: false, showSelectionIndicator: true,
    })
    this.#theme = theme
    this.on(SelectRenderableEvents.ITEM_SELECTED, (index: number) => {
      const action = this.#actions?.[index]
      if (this.#enabled && action !== undefined) this.#activate?.(action.id)
    })
  }
  update(actions: UiPresentation["descriptor"]["actions"] | null, enabled: boolean, activate: ((id: string) => void) | null): void {
    this.#activate = activate
    this.#enabled = enabled
    this.textColor = enabled ? this.#theme.accent : this.#theme.textMuted
    if (this.#actions === actions) return
    const previous = this.getSelectedOption()?.value
    this.#actions = actions
    this.options = actions?.map(action => ({ name: action.label, description: "", value: action.id })) ?? []
    this.height = actions?.length ?? 0
    this.visible = (actions?.length ?? 0) > 0
    this.setSelectedIndex(Math.max(0, actions?.findIndex(action => action.id === previous) ?? 0))
    if (!this.visible) this.blur()
  }
  handleKey(key: KeyEvent): boolean {
    if (!this.focused || key.ctrl || key.meta || key.shift) return false
    if (key.name === "up" || key.name === "down") {
      this.setSelectedIndex(Math.max(0, Math.min((this.#actions?.length ?? 1) - 1,
        this.getSelectedIndex() + (key.name === "up" ? -1 : 1))))
    } else if (key.name === "return" || key.name === "enter") this.selectCurrent()
    else return false
    return true
  }
  override destroyRecursively(): void { this.update(null, false, null); super.destroyRecursively() }
  override destroy(): void { this.update(null, false, null); super.destroy() }
}
