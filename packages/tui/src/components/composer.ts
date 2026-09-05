import {
  BoxRenderable,
  TextRenderable,
  bg,
  fg,
  t,
  type PasteEvent,
  type RenderContext,
} from "@opentui/core"

import {
  MAX_ATTACHMENTS_PER_MESSAGE,
  MAX_IMAGE_ATTACHMENT_BYTES,
  MAX_TEXT_ATTACHMENT_BYTES,
  MAX_TOTAL_ATTACHMENT_BYTES,
  type Attachment,
} from "../protocol"
import type { ClipboardImage, EditorAdapter, ImagePasteAdapter } from "../platform"
import type { RottweilerTheme } from "../theme"
import { ComposerEditorRenderable } from "./composer-editor"
import { jsonEncodedBytes } from "../json-size"
import { ComposerDraftStore, sameAttachment } from "../composer-drafts"
import { ImageAttachmentRenderable } from "./image"

const COMPOSER_PLACEHOLDER = "Describe a task…"

export interface ComposerOptions {
  readonly editor: EditorAdapter
  readonly imagePaste: ImagePasteAdapter
  readonly pasteImageKeycap?: string
  readonly externalEditorKeycap?: string
  readonly onSubmit: (
    content: string,
    attachments: readonly Attachment[],
  ) => boolean | Promise<boolean>
  readonly onFileMention: (mention: ComposerFileMention) => void
  readonly onManageAttachments?: () => void
  readonly onAttachmentError?: (message: string) => void
  readonly submissionScope?: () => string
  readonly drafts?: ComposerDraftStore
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
  readonly editor: ComposerEditorRenderable
  readonly attachmentsText: TextRenderable
  readonly queueText: TextRenderable
  readonly hintText: TextRenderable
  readonly #ownsDrafts: boolean
  readonly #drafts: ComposerDraftStore
  #attachments: Attachment[] = []
  #imagePreview: ImageAttachmentRenderable | null = null
  #options: ComposerOptions
  #theme: RottweilerTheme
  #inputGeneration: object = {}
  #editorRequest: object | null = null
  #submitting = false
  #shellMode = false
  #imagePasteAvailable = false
  #inputMode: "normal" | "insert" | null = null
  #dockHeight = 4
  #history: string[] = []
  #historyIndex: number | null = null
  #historyDraft = ""
  #restoringHistory = false
  #pendingHistoryRestore: string | null = null
  #placeholder: string

  constructor(ctx: RenderContext, theme: RottweilerTheme, options: ComposerOptions) {
    super(ctx, {
      id: "composer",
      width: "auto",
      height: 4,
      minHeight: 4,
      maxHeight: 9,
      flexShrink: 0,
      flexDirection: "column",
      overflow: "hidden",
      border: true,
      borderStyle: "rounded",
      borderColor: theme.border,
      focusedBorderColor: theme.border,
      backgroundColor: theme.background,
      paddingX: 1,
      marginLeft: 1,
      marginRight: 1,
    })
    this.#ownsDrafts = options.drafts === undefined
    this.#drafts = options.drafts ?? new ComposerDraftStore()
    this.#options = options
    this.#theme = theme
    this.#placeholder = COMPOSER_PLACEHOLDER
    this.attachmentsText = new TextRenderable(ctx, {
      id: "composer-attachments",
      content: "",
      fg: theme.info,
      height: 1,
      visible: false,
      onMouseDown: () => this.#options.onManageAttachments?.(),
    })
    this.editor = new ComposerEditorRenderable(ctx, {
      id: "composer-editor",
      width: "100%",
      flexGrow: 1,
      flexShrink: 1,
      minHeight: 1,
      maxHeight: 5,
      initialValue: "",
      placeholder: this.#placeholder,
      backgroundColor: theme.background,
      textColor: theme.text,
      focusedBackgroundColor: theme.background,
      focusedTextColor: theme.text,
      placeholderColor: theme.textMuted,
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
    }, codeUnits => {
      if (this.#drafts.canRetainText(this.#scope(), codeUnits, this.#attachments)) return true
      this.#options.onAttachmentError?.("Draft storage is full. Shorten a draft or remove an attachment before adding more content.")
      return false
    })
    this.queueText = new TextRenderable(ctx, {
      id: "composer-queue",
      content: "",
      fg: theme.warning,
      height: 1,
      visible: false,
    })
    this.hintText = new TextRenderable(ctx, {
      id: "composer-hints",
      content: composerHints(theme, options, false, null),
      fg: theme.textMuted,
      height: 1,
      flexShrink: 0,
      wrapMode: "none",
    })
    this.add(this.attachmentsText)
    this.add(this.editor)
    this.add(this.hintText)
    this.add(this.queueText)
  }

  override destroy(): void {
    if (this.isDestroyed) return
    if (this.#ownsDrafts) this.#drafts.clear()
    super.destroy()
  }

  get value(): string {
    return this.editor.plainText
  }

  set value(value: string) {
    if (!this.#admitDraft(value, this.#attachments)) return
    this.#inputGeneration = {}
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
    // OpenTUI may publish Textarea change notifications after setText returns.
    // Retain the exact restored value so that deferred notification cannot
    // reset the history cursor before the user presses Down.
    this.#pendingHistoryRestore = value
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
    if (!this.#admitDraft(content, attachments)) return
    this.#inputGeneration = {}
    this.editor.setText(content)
    this.#attachments = [...this.#drafts.get(this.#scope()).attachments]
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
    this.focusedBorderColor = active ? this.#theme.warning : this.#theme.border
    this.editor.placeholder = active
      ? "Shell command · Enter to run in foreground"
      : this.#placeholder
  }

  setImagePasteAvailable(available: boolean): void {
    if (this.#imagePasteAvailable === available) return
    this.#imagePasteAvailable = available
    this.hintText.content = composerHints(this.#theme, this.#options, available, this.#inputMode)
  }

  setKeybindingMode(mode: "normal" | "insert" | null): void {
    if (this.#inputMode === mode) return
    this.#inputMode = mode
    this.hintText.content = composerHints(
      this.#theme,
      this.#options,
      this.#imagePasteAvailable,
      mode,
    )
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
    if (!this.#admitDraft(next, this.#attachments)) return false
    this.editor.setText(next)
    this.setShellMode(next.startsWith("!"))
    const cursorCharacters = start + replacement.length
    this.editor.cursorOffset = Buffer.byteLength(next.slice(0, cursorCharacters))
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
    if (this.#attachments.length > MAX_ATTACHMENTS_PER_MESSAGE
      || this.#attachments.reduce((sum, item) => sum + (attachmentBytes(item) ?? 0), 0) > MAX_TOTAL_ATTACHMENT_BYTES
      || composerWireBytes(content, this.#attachments) > MAX_COMPOSER_WIRE_BYTES) {
      this.#options.onAttachmentError?.(
        "This message is too large to send. Remove some text or attachments and try again.",
      )
      return false
    }
    const submissionScope = this.#scope()
    if (!this.#admitDraft(content, this.#attachments)) return false
    const reservation = this.#drafts.submit(submissionScope)
    if (reservation === null) return false
    const submittedAttachments = reservation.draft.attachments
    this.#submitting = true
    this.#inputGeneration = {}
    this.editor.clear()
    this.setShellMode(false)
    this.#attachments = []
    this.#refreshAttachments()
    try {
      const accepted = await this.#options.onSubmit(content, submittedAttachments)
      if (!this.isDestroyed) this.#synchronizeDraft()
      const restored = reservation.settle(accepted)
      if (accepted && !this.isDestroyed) {
        this.#rememberPrompt(content)
        this.#options.onSubmitted?.()
      } else if (!this.isDestroyed && restored !== null && this.#scope() === submissionScope) {
        this.restoreDraft(restored.content, restored.attachments)
      }
      return accepted
    } catch (error) {
      if (!this.isDestroyed) this.#synchronizeDraft()
      const restored = reservation.settle(false)
      if (!this.isDestroyed && restored !== null && this.#scope() === submissionScope) this.restoreDraft(restored.content, restored.attachments)
      throw error
    } finally {
      this.#submitting = false
      if (!this.isDestroyed) this.#options.onSubmissionSettled?.()
    }
  }

  #synchronizeDraft(): void {
    if (this.#admitDraft(this.editor.plainText, this.#attachments)) return
    const retained = this.#drafts.get(this.#scope())
    this.editor.setText(retained.content)
    this.#attachments = [...retained.attachments]
    this.#refreshAttachments()
  }

  #scope(): string { return this.#options.submissionScope?.() ?? "default" }

  #admitDraft(content: string, attachments: readonly Attachment[]): boolean {
    if (this.#drafts.set(this.#scope(), { content, attachments })) return true
    this.#options.onAttachmentError?.("Draft storage is full. Shorten a draft or remove an attachment before adding more content.")
    return false
  }

  addAttachment(attachment: Attachment): boolean {
    if (attachment.media_type.startsWith("image/") && !this.#imagePasteAvailable) {
      this.#options.onAttachmentError?.(
        "The selected model does not support image input. Choose a vision-capable model first.",
      )
      return false
    }
    if (this.#attachments.some(existing => sameAttachment(existing, attachment))) return true
    const admitted = Object.freeze({ ...attachment, data: Object.freeze({ ...attachment.data }) })
    const error = attachmentBudgetError(admitted, this.#attachments, this.editor.plainText)
    if (error !== null) {
      this.#options.onAttachmentError?.(error)
      return false
    }
    const attachments = [...this.#attachments, admitted]
    if (!this.#admitDraft(this.editor.plainText, attachments)) return false
    this.#attachments = [...this.#drafts.get(this.#scope()).attachments]
    this.#refreshAttachments()
    return true
  }

  removeAttachment(index: number): boolean {
    if (index < 0 || index >= this.#attachments.length) return false
    const attachments = this.#attachments.filter((_, candidate) => candidate !== index)
    if (!this.#admitDraft(this.editor.plainText, attachments)) return false
    this.#attachments = [...this.#drafts.get(this.#scope()).attachments]
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
    if (!this.#imagePasteAvailable) return false
    const current = this.#inputOwner()
    let image: ClipboardImage | null
    try { image = await this.#options.imagePaste.readImage() }
    catch (error) { if (current()) throw error; return false }
    if (!current() || image === null) {
      return false
    }
    return this.addImage(image)
  }

  async #paste(event: PasteEvent): Promise<void> {
    if (!this.editor.focused) return
    const current = this.#inputOwner()
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
      const pasted = await this.pasteImage()
      if (!current()) return
      if (!pasted) {
        this.#options.onAttachmentError?.("The clipboard does not contain a supported image.")
      }
      return
    }
    let localImage: ClipboardImage | null
    try {
      localImage = await this.#options.imagePaste.readPath(trimmed)
    } catch (error) {
      if (!current()) return
      this.#options.onAttachmentError?.(
        error instanceof Error ? error.message : "That image path could not be attached safely.",
      )
      return
    }
    if (!current()) return
    if (localImage !== null) {
      this.addImage(localImage)
      return
    }
    const bytes = Buffer.byteLength(trimmed)
    const lineCount = (trimmed.match(/\n/g)?.length ?? 0) + 1
    if ((lineCount >= 3 || trimmed.length > 150) && bytes <= 1024 * 1024) {
      const ordinal = this.#attachments.filter((item) => item.name.startsWith("Pasted text")).length + 1
      this.addAttachment({
        name: `Pasted text ${ordinal}`,
        media_type: "text/plain",
        data: { type: "text", content: trimmed },
      })
      return
    }
    if (bytes > 1024 * 1024) {
      this.#options.onAttachmentError?.("Pasted text exceeds the 1 MiB message attachment limit.")
      return
    }
    this.editor.insertText(normalized)
  }

  #inputOwner(): () => boolean {
    const generation = this.#inputGeneration
    const scope = this.#options.submissionScope?.()
    return () => !this.isDestroyed && generation === this.#inputGeneration
      && scope === this.#options.submissionScope?.()
  }

  async openExternalEditor(): Promise<void> {
    if (this.#editorRequest !== null) return
    const current = this.#inputOwner()
    const request = {}
    this.#editorRequest = request
    const content = this.editor.plainText
    let result: string | null
    try { result = await this.#options.editor.compose(content) }
    catch (error) { if (current()) throw error; return }
    finally { if (this.#editorRequest === request) this.#editorRequest = null }
    if (!current()) return
    if (result !== null) {
      if (!this.#admitDraft(result, this.#attachments)) return
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
    const value = this.editor.plainText
    if (!this.#admitDraft(value, this.#attachments)) {
      const retained = this.#drafts.get(this.#scope())
      this.editor.setText(retained.content)
      this.#attachments = [...retained.attachments]
      this.#refreshAttachments()
      return
    }
    const restoringHistory =
      this.#restoringHistory || this.#pendingHistoryRestore === value
    if (!restoringHistory) {
      this.#pendingHistoryRestore = null
      this.#historyIndex = null
      this.#historyDraft = ""
    }
    this.#refreshHeight()
    this.setShellMode(value.startsWith("!"))
    // Recalling a slash command must not open autocomplete and steal the next
    // Down key. The recalled draft becomes ordinary editable input on the
    // first real content change, which resumes autocomplete normally.
    if (restoringHistory) return
    this.#options.onInput?.(value)
    const mention = this.currentFileMention()
    if (mention !== null) this.#options.onFileMention(mention)
  }

  #rememberPrompt(content: string): void {
    if (content.trim().length === 0) return
    if (this.#history.at(-1) !== content) this.#history.push(content)
    let bytes = this.#history.reduce((sum, prompt) => sum + 32 + prompt.length * 2, 0)
    while (this.#history.length > 256 || bytes > 1024 * 1024) {
      const oldest = this.#history.shift()
      if (oldest === undefined) break
      bytes -= 32 + oldest.length * 2
    }
    this.#historyIndex = null
    this.#historyDraft = ""
    this.#pendingHistoryRestore = null
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
    const editorWidth = Math.max(
      1,
      this.editor.width > 0
        ? this.editor.width
        : this.width > 4
          ? this.width - 4
          : this.ctx.width - 4,
    )
    const wrappedRows = estimateWrappedRows(this.editor.plainText, editorWidth)
    const minimumEditorRows = 1
    this.editor.minHeight = minimumEditorRows
    const editorRows = Math.min(
      5,
      Math.max(minimumEditorRows, this.editor.lineCount, this.editor.virtualLineCount, wrappedRows),
    )
    this.hintText.visible = terminalLimit >= 4
    this.hintText.height = this.hintText.visible ? 1 : 0
    const fixedExtras =
      (this.attachmentsText.visible ? 1 : 0) +
      (this.queueText.visible ? 1 : 0) +
      (this.hintText.visible ? 1 : 0)
    const imageRows = this.#imagePreview?.height ?? 0
    // Preserve at least one editable row after borders and labels. A preview is
    // decorative and must collapse before it can cover the active textarea.
    const imageVisible = imageRows > 0 && terminalLimit - 2 - fixedExtras - 1 >= imageRows
    if (this.#imagePreview !== null) this.#imagePreview.visible = imageVisible
    const extras = fixedExtras + (imageVisible ? imageRows : 0)
    const minimumDockRows = this.ctx.height <= 8 ? 3 : 4
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

function composerHints(
  theme: RottweilerTheme,
  options: Pick<ComposerOptions, "pasteImageKeycap" | "externalEditorKeycap">,
  imagePasteAvailable: boolean,
  inputMode: "normal" | "insert" | null,
): ReturnType<typeof t> {
  const editor = options.externalEditorKeycap === undefined
    ? ""
    : `   ${options.externalEditorKeycap} editor`
  const image = !imagePasteAvailable || options.pasteImageKeycap === undefined
    ? ""
    : `   ${options.pasteImageKeycap} image`
  const mode = inputMode === null
    ? ""
    : bg(inputMode === "normal" ? theme.success : theme.primary)(
        fg(theme.background)(` ${inputMode.toUpperCase()} `),
      )
  return t`${mode}${inputMode === null ? "" : fg(theme.textMuted)("  ")}${fg(theme.textMuted)("/ commands   @ files   ! shell")}${editor === "" ? "" : fg(theme.textMuted)(editor)}${image === "" ? "" : fg(theme.textMuted)(image)}${fg(theme.textMuted)("   ")}${bg(theme.backgroundElement)(fg(theme.borderActive)(" ⏎ "))}${fg(theme.textMuted)(" send")}`
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
  let bytes = 0
  let index = 0
  for (const character of value) {
    const next = bytes + Buffer.byteLength(character)
    if (next > target) break
    bytes = next
    index += character.length
  }
  return index
}

// The host accepts a 16 MiB JSON command. Keep one MiB for the command envelope,
// session identity, and future additive fields while measuring the exact UTF-8
// JSON representation of all user-controlled composer payloads.
const MAX_COMPOSER_WIRE_BYTES = 15 * 1024 * 1024

function attachmentBudgetError(
  attachment: Attachment,
  current: readonly Attachment[],
  content = "",
): string | null {
  if (current.length >= MAX_ATTACHMENTS_PER_MESSAGE) {
    return `A message can include at most ${MAX_ATTACHMENTS_PER_MESSAGE} attachments.`
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

const attachmentWireSizes = new WeakMap<Attachment, number>()
function composerWireBytes(content: string, attachments: readonly Attachment[]): number {
  let bytes = jsonEncodedBytes({ content, attachments: [] }, MAX_COMPOSER_WIRE_BYTES)
  for (const [index, attachment] of attachments.entries()) {
    if (bytes > MAX_COMPOSER_WIRE_BYTES) return bytes
    let encoded = attachmentWireSizes.get(attachment)
    if (encoded === undefined) {
      encoded = attachment.data.type === "inline_base64" && attachmentBytes(attachment) !== null
        ? jsonEncodedBytes({ ...attachment, data: { type: "inline_base64", data: "" } }, MAX_COMPOSER_WIRE_BYTES) + attachment.data.data.length
        : jsonEncodedBytes(attachment, MAX_COMPOSER_WIRE_BYTES)
      if (Object.isFrozen(attachment) && Object.isFrozen(attachment.data)) attachmentWireSizes.set(attachment, encoded)
    }
    bytes += encoded + (index > 0 ? 1 : 0)
  }
  return bytes
}

const attachmentContentSizes = new WeakMap<Attachment, number>()
function attachmentBytes(attachment: Attachment): number | null {
  const cached = attachmentContentSizes.get(attachment)
  if (cached !== undefined) return cached
  if (attachment.data.type === "text") {
    const bytes = Buffer.byteLength(attachment.data.content)
    if (Object.isFrozen(attachment) && Object.isFrozen(attachment.data)) attachmentContentSizes.set(attachment, bytes)
    return bytes
  }
  const value = attachment.data.data
  if (
    value.length === 0 ||
    value.length % 4 !== 0 ||
    !/^[A-Za-z0-9+/]*={0,2}$/.test(value)
  ) return null
  const padding = value.endsWith("==") ? 2 : value.endsWith("=") ? 1 : 0
  const bytes = (value.length / 4) * 3 - padding
  if (Object.isFrozen(attachment) && Object.isFrozen(attachment.data)) attachmentContentSizes.set(attachment, bytes)
  return bytes
}
