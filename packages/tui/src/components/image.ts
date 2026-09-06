import { TextRenderable } from "./text"
import { BoxRenderable, type RenderContext } from "@opentui/core"

import type { Attachment } from "../protocol"
import type { RottweilerTheme } from "../theme"

export class ImageAttachmentRenderable extends BoxRenderable {
  readonly preview: TextRenderable

  constructor(ctx: RenderContext, theme: RottweilerTheme, attachment: Attachment) {
    const graphicsCapable =
      ctx.capabilities?.kitty_graphics === true || ctx.capabilities?.sixel === true
    super(ctx, {
      id: `image-${attachment.name}`,
      width: "100%",
      height: graphicsCapable ? 4 : 2,
      border: true,
      borderStyle: "single",
      borderColor: theme.border,
      backgroundColor: theme.backgroundPanel,
      paddingX: 1,
    })
    this.preview = new TextRenderable(ctx, {
      content: graphicsCapable
        ? `▧▨▩ ${attachment.name}\n▨▩▧ cell preview · ${attachment.media_type}`
        : `🖼 ${attachment.name} · ${attachment.media_type} · preview unavailable`,
      fg: graphicsCapable ? theme.info : theme.textMuted,
      wrapMode: "word",
    })
    this.add(this.preview)
  }
}
