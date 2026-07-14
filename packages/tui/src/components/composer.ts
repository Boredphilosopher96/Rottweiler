import {
  BoxRenderable,
  TextRenderable,
  TextareaRenderable,
  type PasteEvent,
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
  readonly onFileMention: (mention: ComposerFileMention) => void
  readonly onManageAttachments?: () => void
  readonly onAttachmentError?: (message: string) => void
  readonly submissionScope?: () => string
  readonly onDetachedSubmissionRejected?: (
    scope: string,
    content: string,
    attachments: readonly Attachment[],
  ) => void
  readonly onInput?: (value: string) => void
  readonly onSubmitted?: () => void
  readonly onSubmissionSettled?: () => void
  readonly onHeightChange?: (height: number) => void
}

export interface ComposerFileMention {
  readonly query: string
  readonly start: number
  readonly end: number
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
  #shellMode = false
  #dockHeight = 4
  #history: string[] = []
  #historyIndex: number | null = null
  #historyDraft = ""
  #restoringHistory = false

  constructor(ctx: RenderContext, theme: RottweilerTheme, options: ComposerOptions) {
    super(ctx, {
      id: "composer",
      width: "100%",
      height: 4,
      minHeight: 4,
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
      onMouseDown: () => this.#options.onManageAttachments?.(),
    })
    this.editor = new TextareaRenderable(ctx, {
      id: "composer-editor",
      width: "100%",
      flexGrow: 1,
      flexShrink: 1,
      minHeight: 2,
      maxHeight: 5,
      initialValue: "",
      placeholder: "Message Rottweiler · @ files · ctrl+v image · ctrl+e $EDITOR",
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
      onPaste: (event) => void this.#paste(event),
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
    this.setShellMode(value.startsWith("!"))
    this.#refreshHeight()
  }

  /**
   * Recall accepted prompts without stealing Up/Down from multiline editing.
   * History starts only at the first/last logical line, matching shell-style
   * composer behavior while preserving the draft that was being written.
   */
  navigateHistory(direction: "previous" | "next"): boolean {
    if (this.#history.length === 0) return false
    const cursor = this.editor.logicalCursor
    if (direction === "previous") {
      if (cursor.row !== 0) return false
      if (this.#historyIndex === null) {
        this.#historyDraft = this.editor.plainText
        this.#historyIndex = this.#history.length - 1
      } else if (this.#historyIndex > 0) {
        this.#historyIndex -= 1
      }
    } else {
      if (cursor.row !== this.editor.lineCount - 1 || this.#historyIndex === null) return false
      if (this.#historyIndex < this.#history.length - 1) {
        this.#historyIndex += 1
      } else {
        this.#historyIndex = null
      }
    }
    const value = this.#historyIndex === null
      ? this.#historyDraft
      : (this.#history[this.#historyIndex] ?? "")
    this.#restoringHistory = true
    try {
      this.editor.setText(value)
      this.editor.gotoBufferEnd()
      this.setShellMode(value.startsWith("!"))
      this.#refreshHeight()
    } finally {
      this.#restoringHistory = false
    }
    return true
  }

  get attachments(): readonly Attachment[] {
    return this.#attachments
  }

  /** Replace the visible draft when switching between parent and child sessions. */
  restoreDraft(content: string, attachments: readonly Attachment[]): void {
    this.editor.setText(content)
    this.#attachments = [...attachments]
    this.setShellMode(content.startsWith("!"))
    this.#refreshAttachments()
  }

  get dockHeight(): number {
    return this.visible ? this.#dockHeight : 0
  }

  get shellMode(): boolean {
    return this.#shellMode
  }

  setShellMode(active: boolean): void {
    if (this.#shellMode === active) return
    this.#shellMode = active
    this.title = active ? " Shell " : ""
    this.borderColor = active ? this.#theme.warning : this.#theme.border
    this.focusedBorderColor = active ? this.#theme.warning : this.#theme.focus
    this.editor.placeholder = active
      ? "Shell command · Enter to run in foreground"
      : "Message Rottweiler · @ files · ctrl+v image · ctrl+e $EDITOR"
  }

  currentFileMention(): ComposerFileMention | null {
    const value = this.editor.plainText
    const cursor = characterIndexForByteOffset(value, this.editor.cursorOffset)
    const prefix = value.slice(0, cursor)
    const start = prefix.lastIndexOf("@")
    if (
      start < 0 ||
      (start > 0 && !/\s/.test(prefix[start - 1] ?? "")) ||
      prefix.slice(start + 1).includes("\n")
    ) return null
    return { query: prefix.slice(start + 1), start, end: prefix.length }
  }

  replaceRange(start: number, end: number, replacement: string): boolean {
    const value = this.editor.plainText
    if (start < 0 || end < start || end > value.length) return false
    const next = value.slice(0, start) + replacement + value.slice(end)
    this.editor.setText(next)
    this.setShellMode(next.startsWith("!"))
    const cursorCharacters = start + replacement.length
    this.editor.cursorOffset = new TextEncoder().encode(next.slice(0, cursorCharacters)).length
    this.#refreshHeight()
    return true
  }

  override focus(): void {
    this.editor.focus()
  }

  async submit(): Promise<boolean> {
    const content = this.editor.plainText
    if ((content.trim().length === 0 && this.#attachments.length === 0) || this.#submitting) {
      return false
    }
    if (composerWireBytes(content, this.#attachments) > MAX_COMPOSER_WIRE_BYTES) {
      this.#options.onAttachmentError?.(
        "This message is too large to send. Remove some text or attachments and try again.",
      )
      return false
    }
    const submittedAttachments = this.#attachments
    const submissionScope = this.#options.submissionScope?.() ?? "default"
    this.#submitting = true
    this.editor.clear()
    this.setShellMode(false)
    this.#attachments = []
    this.#refreshAttachments()
    try {
      const accepted = await this.#options.onSubmit(content, submittedAttachments)
      if (accepted) {
        this.#rememberPrompt(content)
        this.#options.onSubmitted?.()
      } else {
        this.#restoreRejectedSubmissionForScope(submissionScope, content, submittedAttachments)
      }
      return accepted
    } catch (error) {
      this.#restoreRejectedSubmissionForScope(submissionScope, content, submittedAttachments)
      throw error
    } finally {
      this.#submitting = false
      this.#options.onSubmissionSettled?.()
    }
  }

  #restoreRejectedSubmissionForScope(
    scope: string,
    content: string,
    attachments: readonly Attachment[],
  ): void {
    if ((this.#options.submissionScope?.() ?? "default") === scope) {
      this.#restoreRejectedSubmission(content, attachments)
    } else {
      this.#options.onDetachedSubmissionRejected?.(scope, content, attachments)
    }
  }

  #restoreRejectedSubmission(content: string, attachments: readonly Attachment[]): void {
    const current = this.editor.plainText
    this.editor.setText(current.length === 0 ? content : `${content}\n${current}`)
    this.setShellMode(this.editor.plainText.startsWith("!"))
    const merged: Attachment[] = []
    const identities = new Set<string>()
    for (const attachment of this.#attachments) {
      const identity = attachmentIdentity(attachment)
      if (identities.has(identity)) continue
      identities.add(identity)
      merged.push(attachment)
    }
    let overflow = false
    for (const attachment of attachments) {
      const identity = attachmentIdentity(attachment)
      if (identities.has(identity)) continue
      if (
        merged.length < MAX_ATTACHMENTS &&
        attachmentBudgetError(attachment, merged, this.editor.plainText) === null
      ) {
        identities.add(identity)
        merged.push(attachment)
      } else {
        overflow = true
      }
    }
    if (overflow) {
      this.#options.onAttachmentError?.(
        "Some attachments from the rejected send could not be restored because the current draft reached its attachment limit.",
      )
    }
    this.#attachments = merged
    this.#refreshAttachments()
  }

  addAttachment(attachment: Attachment): boolean {
    const identity = attachmentIdentity(attachment)
    if (this.#attachments.some((existing) => attachmentIdentity(existing) === identity)) return true
    const error = attachmentBudgetError(attachment, this.#attachments, this.editor.plainText)
    if (error !== null) {
      this.#options.onAttachmentError?.(error)
      return false
    }
    this.#attachments = [...this.#attachments, attachment]
    this.#refreshAttachments()
    return true
  }

  removeAttachment(index: number): boolean {
    if (index < 0 || index >= this.#attachments.length) return false
    this.#attachments = this.#attachments.filter((_, candidate) => candidate !== index)
    this.#refreshAttachments()
    this.editor.focus()
    return true
  }

  removeLastAttachment(): boolean {
    return this.removeAttachment(this.#attachments.length - 1)
  }

  addImage(image: ClipboardImage): boolean {
    return this.addAttachment({
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
    return this.addImage(image)
  }

  async #paste(event: PasteEvent): Promise<void> {
    if (!this.editor.focused) return
    let text: string
    try {
      text = new TextDecoder("utf-8", { fatal: true }).decode(event.bytes)
    } catch {
      event.preventDefault()
      this.#options.onAttachmentError?.("The pasted content is not valid UTF-8 text.")
      return
    }
    event.preventDefault()
    const normalized = text.replace(/\r\n/g, "\n").replace(/\r/g, "\n")
    const trimmed = normalized.trim()
    if (trimmed.length === 0) {
      if (!(await this.pasteImage())) {
        this.#options.onAttachmentError?.("The clipboard does not contain a supported image.")
      }
      return
    }
    let localImage: ClipboardImage | null
    try {
      localImage = await this.#options.imagePaste.readPath(trimmed)
    } catch (error) {
      this.#options.onAttachmentError?.(
        error instanceof Error ? error.message : "That image path could not be attached safely.",
      )
      return
    }
    if (localImage !== null) {
      this.addImage(localImage)
      return
    }
    const bytes = new TextEncoder().encode(trimmed)
    const lineCount = (trimmed.match(/\n/g)?.length ?? 0) + 1
    if ((lineCount >= 3 || trimmed.length > 150) && bytes.length <= 1024 * 1024) {
      const ordinal = this.#attachments.filter((item) => item.name.startsWith("Pasted text")).length + 1
      this.addAttachment({
        name: `Pasted text ${ordinal}`,
        media_type: "text/plain",
        data: { type: "text", content: trimmed },
      })
      return
    }
    if (bytes.length > 1024 * 1024) {
      this.#options.onAttachmentError?.("Pasted text exceeds the 1 MiB message attachment limit.")
      return
    }
    this.editor.insertText(normalized)
  }

  async openExternalEditor(): Promise<void> {
    const result = await this.#options.editor.compose(this.editor.plainText)
    if (result !== null) {
      this.editor.replaceText(result)
      this.setShellMode(result.startsWith("!"))
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
    if (!this.#restoringHistory) {
      this.#historyIndex = null
      this.#historyDraft = ""
    }
    this.#refreshHeight()
    const value = this.editor.plainText
    this.setShellMode(value.startsWith("!"))
    this.#options.onInput?.(value)
    const mention = this.currentFileMention()
    if (mention !== null) this.#options.onFileMention(mention)
  }

  #rememberPrompt(content: string): void {
    if (content.trim().length === 0) return
    if (this.#history.at(-1) !== content) this.#history.push(content)
    if (this.#history.length > 256) this.#history.splice(0, this.#history.length - 256)
    this.#historyIndex = null
    this.#historyDraft = ""
  }

  #refreshAttachments(): void {
    this.attachmentsText.visible = this.#attachments.length > 0
    this.attachmentsText.content = this.#attachments
      .map((attachment, index) =>
        `▣ ${index + 1}:${attachment.source_path ?? attachment.name}`
      )
      .join("  ") + "  · click to manage · Backspace removes last"
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
    const compactTerminal = this.ctx.height <= 8
    const editorWidth = Math.max(
      1,
      this.editor.width > 0
        ? this.editor.width
        : this.width > 4
          ? this.width - 4
          : this.ctx.width - 4,
    )
    const wrappedRows = estimateWrappedRows(this.editor.plainText, editorWidth)
    const minimumEditorRows = compactTerminal || terminalLimit < 4 ? 1 : 2
    this.editor.minHeight = minimumEditorRows
    const editorRows = Math.min(
      5,
      Math.max(minimumEditorRows, this.editor.lineCount, this.editor.virtualLineCount, wrappedRows),
    )
    const fixedExtras =
      (this.attachmentsText.visible ? 1 : 0) + (this.queueText.visible ? 1 : 0)
    const imageRows = this.#imagePreview?.height ?? 0
    // Preserve at least one editable row after borders and labels. A preview is
    // decorative and must collapse before it can cover the active textarea.
    const imageVisible = imageRows > 0 && terminalLimit - 2 - fixedExtras - 1 >= imageRows
    if (this.#imagePreview !== null) this.#imagePreview.visible = imageVisible
    const extras = fixedExtras + (imageVisible ? imageRows : 0)
    const minimumDockRows = compactTerminal ? 3 : 4
    this.minHeight = minimumDockRows
    const nextHeight = Math.min(
      terminalLimit,
      Math.max(minimumDockRows, 2 + editorRows + extras),
    )
    const heightChanged = this.#dockHeight !== nextHeight
    this.#dockHeight = nextHeight
    this.height = nextHeight
    if (heightChanged) this.#options.onHeightChange?.(nextHeight)
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

function characterIndexForByteOffset(value: string, target: number): number {
  if (target <= 0) return 0
  const encoder = new TextEncoder()
  let bytes = 0
  let index = 0
  for (const character of value) {
    const next = bytes + encoder.encode(character).length
    if (next > target) break
    bytes = next
    index += character.length
  }
  return index
}

function attachmentIdentity(attachment: Attachment): string {
  if (attachment.source_path !== undefined) return `path:${attachment.source_path}`
  if (attachment.data.type === "inline_base64") {
    return `image:${attachment.media_type}:${attachment.data.data}`
  }
  return `text:${attachment.media_type}:${attachment.data.content}`
}

const MAX_ATTACHMENTS = 16
const MAX_TEXT_ATTACHMENT_BYTES = 1024 * 1024
const MAX_IMAGE_ATTACHMENT_BYTES = 5 * 1024 * 1024
const MAX_TOTAL_ATTACHMENT_BYTES = 10 * 1024 * 1024
// The host accepts a 16 MiB JSON command. Keep one MiB for the command envelope,
// session identity, and future additive fields while measuring the exact UTF-8
// JSON representation of all user-controlled composer payloads.
const MAX_COMPOSER_WIRE_BYTES = 15 * 1024 * 1024

function attachmentBudgetError(
  attachment: Attachment,
  current: readonly Attachment[],
  content = "",
): string | null {
  if (current.length >= MAX_ATTACHMENTS) {
    return `A message can include at most ${MAX_ATTACHMENTS} attachments.`
  }
  const bytes = attachmentBytes(attachment)
  if (bytes === null) return "That attachment has invalid image data."
  const itemLimit = attachment.data.type === "text"
    ? MAX_TEXT_ATTACHMENT_BYTES
    : MAX_IMAGE_ATTACHMENT_BYTES
  if (bytes > itemLimit) {
    return attachment.data.type === "text"
      ? "A text attachment can be at most 1 MiB."
      : "An image attachment can be at most 5 MiB."
  }
  const total = current.reduce((sum, item) => sum + (attachmentBytes(item) ?? 0), bytes)
  if (total > MAX_TOTAL_ATTACHMENT_BYTES) {
    return "Attachments in one message can total at most 10 MiB."
  }
  return composerWireBytes(content, [...current, attachment]) > MAX_COMPOSER_WIRE_BYTES
    ? "That attachment would make this message too large to send."
    : null
}

function composerWireBytes(content: string, attachments: readonly Attachment[]): number {
  return new TextEncoder().encode(JSON.stringify({ content, attachments })).length
}

function attachmentBytes(attachment: Attachment): number | null {
  if (attachment.data.type === "text") {
    return new TextEncoder().encode(attachment.data.content).length
  }
  const value = attachment.data.data
  if (
    value.length === 0 ||
    value.length % 4 !== 0 ||
    !/^[A-Za-z0-9+/]*={0,2}$/.test(value)
  ) return null
  const padding = value.endsWith("==") ? 2 : value.endsWith("=") ? 1 : 0
  return (value.length / 4) * 3 - padding
}
