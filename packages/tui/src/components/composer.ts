import {
  BoxRenderable,
  TextRenderable,
  TextareaRenderable,
  type RenderContext,
} from "@opentui/core"

import type { Attachment } from "../protocol"
import type { ClipboardImage, EditorAdapter, ImagePasteAdapter } from "../platform"
import type { RottweilerTheme } from "../theme"
import { ImageAttachmentRenderable } from "./image"

export interface ComposerOptions {
  readonly editor: EditorAdapter
  readonly imagePaste: ImagePasteAdapter
  readonly onSubmit: (content: string, attachments: readonly Attachment[]) => void
  readonly onFileMention: (query: string) => void
  readonly onInput?: (value: string) => void
}

export class ComposerRenderable extends BoxRenderable {
  readonly editor: TextareaRenderable
  readonly attachmentsText: TextRenderable
  readonly queueText: TextRenderable
  #attachments: Attachment[] = []
  #imagePreview: ImageAttachmentRenderable | null = null
  #options: ComposerOptions
  #theme: RottweilerTheme

  constructor(ctx: RenderContext, theme: RottweilerTheme, options: ComposerOptions) {
    super(ctx, {
      id: "composer",
      width: "100%",
      minHeight: 4,
      maxHeight: 9,
      flexDirection: "column",
      border: true,
      borderStyle: "rounded",
      borderColor: theme.border,
      focusedBorderColor: theme.focus,
      backgroundColor: theme.panel,
      paddingX: 1,
    })
    this.#options = options
    this.#theme = theme
    this.attachmentsText = new TextRenderable(ctx, {
      id: "composer-attachments",
      content: "",
      fg: theme.info,
      height: 1,
      visible: false,
    })
    this.editor = new TextareaRenderable(ctx, {
      id: "composer-editor",
      width: "100%",
      flexGrow: 1,
      initialValue: "",
      placeholder: "Message Rottweiler · @ files · ctrl+e $EDITOR",
      backgroundColor: theme.panel,
      textColor: theme.foreground,
      focusedBackgroundColor: theme.panelRaised,
      focusedTextColor: theme.foreground,
      placeholderColor: theme.subtle,
      wrapMode: "word",
      onSubmit: () => this.submit(),
      onContentChange: () => this.#contentChanged(),
    })
    this.queueText = new TextRenderable(ctx, {
      id: "composer-queue",
      content: "",
      fg: theme.warning,
      height: 1,
      visible: false,
    })
    this.editor.onKeyDown = (key) => {
      if (key.ctrl && key.name === "e") {
        key.preventDefault()
        void this.openExternalEditor()
      }
    }
    this.add(this.attachmentsText)
    this.add(this.editor)
    this.add(this.queueText)
  }

  get value(): string {
    return this.editor.plainText
  }

  set value(value: string) {
    this.editor.setText(value)
  }

  get attachments(): readonly Attachment[] {
    return this.#attachments
  }

  override focus(): void {
    this.editor.focus()
  }

  submit(): void {
    const content = this.editor.plainText.trim()
    if (content.length === 0 && this.#attachments.length === 0) {
      return
    }
    this.#options.onSubmit(content, this.#attachments)
    this.editor.clear()
    this.#attachments = []
    this.#refreshAttachments()
  }

  addAttachment(attachment: Attachment): void {
    this.#attachments = [...this.#attachments, attachment]
    this.#refreshAttachments()
  }

  addImage(image: ClipboardImage): void {
    this.addAttachment({
      name: image.name,
      media_type: image.mediaType,
      data: { type: "inline_base64", data: image.base64 },
    })
  }

  async pasteImage(): Promise<boolean> {
    const image = await this.#options.imagePaste.readImage()
    if (image === null) {
      return false
    }
    this.addImage(image)
    return true
  }

  async openExternalEditor(): Promise<void> {
    const result = await this.#options.editor.compose(this.editor.plainText)
    if (result !== null) {
      this.editor.replaceText(result)
      this.editor.focus()
    }
  }

  setQueuedMessages(messages: readonly { content: string }[]): void {
    this.queueText.visible = messages.length > 0
    this.queueText.content =
      messages.length === 0
        ? ""
        : `Queued ${messages.length} · ${messages.map((message) => message.content).join(" · ")}`
  }

  #contentChanged(): void {
    const value = this.editor.plainText
    this.#options.onInput?.(value)
    const mention = /(?:^|\s)@([^\s]*)$/.exec(value)
    if (mention !== null) {
      this.#options.onFileMention(mention[1] ?? "")
    }
  }

  #refreshAttachments(): void {
    this.attachmentsText.visible = this.#attachments.length > 0
    this.attachmentsText.content = this.#attachments
      .map((attachment) => `▣ ${attachment.name} (${attachment.media_type})`)
      .join("  ")
    if (this.#imagePreview !== null) {
      this.remove(this.#imagePreview)
      this.#imagePreview.destroyRecursively()
      this.#imagePreview = null
    }
    const image = this.#attachments.find((attachment) => attachment.media_type.startsWith("image/"))
    if (image !== undefined) {
      this.#imagePreview = new ImageAttachmentRenderable(this.ctx, this.#theme, image)
      this.insertBefore(this.#imagePreview, this.editor)
    }
  }
}
