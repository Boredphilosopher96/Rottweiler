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
  readonly onSubmit: (
    content: string,
    attachments: readonly Attachment[],
  ) => boolean | Promise<boolean>
  readonly onFileMention: (query: string) => void
  readonly onInput?: (value: string) => void
  readonly onSubmitted?: () => void
}

export class ComposerRenderable extends BoxRenderable {
  readonly editor: TextareaRenderable
  readonly attachmentsText: TextRenderable
  readonly queueText: TextRenderable
  #attachments: Attachment[] = []
  #imagePreview: ImageAttachmentRenderable | null = null
  #options: ComposerOptions
  #theme: RottweilerTheme
  #submitting = false

  constructor(ctx: RenderContext, theme: RottweilerTheme, options: ComposerOptions) {
    super(ctx, {
      id: "composer",
      width: "100%",
      height: 4,
      minHeight: 3,
      maxHeight: 9,
      flexShrink: 0,
      flexDirection: "column",
      overflow: "hidden",
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
      flexShrink: 1,
      minHeight: 1,
      maxHeight: 5,
      initialValue: "",
      placeholder: "Message Rottweiler · @ files · ctrl+e $EDITOR",
      backgroundColor: theme.panel,
      textColor: theme.foreground,
      focusedBackgroundColor: theme.panelRaised,
      focusedTextColor: theme.foreground,
      placeholderColor: theme.subtle,
      wrapMode: "word",
      scrollMargin: 0,
      keyBindings: [
        // OpenCode-style composer semantics: plain Enter sends; modified Enter
        // remains available for drafting multiline prompts.
        { name: "return", action: "submit" },
        { name: "kpenter", action: "submit" },
        { name: "return", shift: true, action: "newline" },
        { name: "return", ctrl: true, action: "newline" },
        { name: "return", meta: true, action: "newline" },
        { name: "kpenter", shift: true, action: "newline" },
        { name: "kpenter", ctrl: true, action: "newline" },
        { name: "kpenter", meta: true, action: "newline" },
        // Raw terminals encode Ctrl+J as LF; Kitty keyboard reports Ctrl+J.
        { name: "linefeed", action: "newline" },
        { name: "j", ctrl: true, action: "newline" },
      ],
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
    this.add(this.attachmentsText)
    this.add(this.editor)
    this.add(this.queueText)
  }

  get value(): string {
    return this.editor.plainText
  }

  set value(value: string) {
    this.editor.setText(value)
    this.#refreshHeight()
  }

  get attachments(): readonly Attachment[] {
    return this.#attachments
  }

  override focus(): void {
    this.editor.focus()
  }

  async submit(): Promise<boolean> {
    const content = this.editor.plainText.trim()
    if ((content.length === 0 && this.#attachments.length === 0) || this.#submitting) {
      return false
    }
    const submittedAttachments = this.#attachments
    this.#submitting = true
    try {
      const accepted = await this.#options.onSubmit(content, submittedAttachments)
      if (
        accepted &&
        this.editor.plainText.trim() === content &&
        this.#attachments === submittedAttachments
      ) {
        this.editor.clear()
        this.#attachments = []
        this.#refreshAttachments()
        this.#options.onSubmitted?.()
      }
      return accepted
    } finally {
      this.#submitting = false
    }
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
    this.#refreshHeight()
  }

  resizeForTerminal(_height: number): void {
    this.#refreshHeight()
  }

  #contentChanged(): void {
    this.#refreshHeight()
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
    this.#refreshHeight()
  }

  #refreshHeight(): void {
    const terminalLimit = Math.max(3, Math.min(9, this.ctx.height - 3))
    const editorWidth = Math.max(
      1,
      this.editor.width > 0
        ? this.editor.width
        : this.width > 4
          ? this.width - 4
          : this.ctx.width - 4,
    )
    const wrappedRows = estimateWrappedRows(this.editor.plainText, editorWidth)
    const editorRows = Math.min(
      5,
      Math.max(1, this.editor.lineCount, this.editor.virtualLineCount, wrappedRows),
    )
    const fixedExtras =
      (this.attachmentsText.visible ? 1 : 0) + (this.queueText.visible ? 1 : 0)
    const imageRows = this.#imagePreview?.height ?? 0
    // Preserve at least one editable row after borders and labels. A preview is
    // decorative and must collapse before it can cover the active textarea.
    const imageVisible = imageRows > 0 && terminalLimit - 2 - fixedExtras - 1 >= imageRows
    if (this.#imagePreview !== null) this.#imagePreview.visible = imageVisible
    const extras = fixedExtras + (imageVisible ? imageRows : 0)
    this.height = Math.min(terminalLimit, Math.max(3, 2 + editorRows + extras))
  }
}

function estimateWrappedRows(value: string, columns: number): number {
  if (value.length === 0) return 1
  return value.split("\n").reduce((rows, line) => {
    // OpenTUI remains the source of truth once its EditorView has laid out.
    // This immediate estimate lets the Yoga parent grow in the same input tick
    // instead of waiting for a second render, including one long logical line.
    const cells = Array.from(line).length
    return rows + Math.max(1, Math.ceil(cells / columns))
  }, 0)
}
