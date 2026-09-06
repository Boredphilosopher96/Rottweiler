import { TextareaRenderable, resolveRenderLib, type RenderContext, type TextareaOptions } from "@opentui/core"

import type { ClientAllocationOwner, ClientAllocationLease } from "../client-allocation"
import { MAX_COMPOSER_TEXT_BYTES } from "../composer-drafts"

export const MAX_COMPOSER_UNDO_BYTES = 4 * 1024 * 1024

/** Bound both native text growth and edit-history retention at their mutation owner. */
export class ComposerEditorRenderable extends TextareaRenderable {
  #canRetain: ((codeUnits: number, utf8Bytes: number) => boolean) | undefined
  #historyBytes = 0
  #editing = false
  #text: string | null = null
  #edits = 0
  readonly #historyAllocation: ClientAllocationLease
  readonly #maximumHistoryBytes: number
  constructor(ctx: RenderContext, options: TextareaOptions, canRetain: (codeUnits: number, utf8Bytes: number) => boolean,
    allocations: ClientAllocationOwner, maximumHistoryBytes = MAX_COMPOSER_UNDO_BYTES) {
    const historyAllocation = allocations.reserve("drafts", maximumHistoryBytes)
    let editor: ComposerEditorRenderable | undefined
    try { super(ctx, { ...options, onContentChange: event => {
      if (editor !== undefined) editor.#text = null
      options.onContentChange?.(event)
    } }) } catch (error) { historyAllocation.release(); throw error }
    this.#historyAllocation = historyAllocation
    editor = this
    this.#canRetain = canRetain
    this.#maximumHistoryBytes = maximumHistoryBytes
  }
  /** Native convenience getters cap reads at 1 MiB; editing admission owns the full buffer. */
  #readText(selected: boolean): string {
    const lib = resolveRenderLib()
    const bytes = lib.textBufferGetByteSize(lib.editBufferGetTextBuffer(this.editBuffer.ptr))
    if (bytes === 0) return ""
    const text = selected ? lib.editorViewGetSelectedTextBytes(this.editorView.ptr, bytes)
      : lib.editBufferGetText(this.editBuffer.ptr, bytes)
    return text === null ? "" : lib.decoder.decode(text)
  }
  override get plainText(): string { return this.#text ??= this.#readText(false) }
  override getSelectedText(): string { return this.getSelection() === null ? "" : this.#readText(true) }
  #nativeBytes(): number {
    const lib = resolveRenderLib()
    return lib.textBufferGetByteSize(lib.editBufferGetTextBuffer(this.editBuffer.ptr))
  }
  override destroy(): void { this.#text = null; try { super.destroy() } finally { this.#historyAllocation.release() } }
  get historyCharge(): number { return this.#historyBytes }
  #admit(length: number, bytes: number): boolean {
    const allowed = this.#canRetain?.(length, bytes) ?? true
    return bytes <= MAX_COMPOSER_TEXT_BYTES && allowed
  }
  #insertion(text: string): boolean {
    const selected = this.getSelectedText()
    return this.#admit(this.plainText.length - selected.length + text.length, this.#nativeBytes() - Buffer.byteLength(selected) + Buffer.byteLength(text))
  }
  #checkpoint(): void {
    const text = this.plainText
    const cursor = this.cursorOffset
    const selection = this.getSelection()
    // setText releases the native add-buffer as well as its undo records.
    this.#text = null
    super.setText(text)
    this.#text = null
    this.cursorOffset = cursor
    if (selection !== null) this.setSelection(selection.start, selection.end)
    this.#historyBytes = 0
    this.#edits = 0
  }
  #edit<T>(insertedLength: number, action: () => T): T {
    if (this.#editing) return action()
    const before = this.#nativeBytes()
    // Persistent rope history shares the unchanged document. Charge inserted add-buffer
    // bytes, deleted retained bytes and per-operation nodes, not the entire base each key.
    const insertionCharge = 512 + insertedLength * 6
    if (this.#edits >= 256 || this.#historyBytes + insertionCharge > this.#maximumHistoryBytes) this.#checkpoint()
    this.#editing = true
    this.#text = null
    try { return action() }
    finally {
      this.#editing = false
      this.#text = null
      this.#edits++
      this.#historyBytes += insertionCharge + Math.max(0, before - this.#nativeBytes()) * 2
      if (this.#historyBytes > this.#maximumHistoryBytes) this.#checkpoint()
    }
  }
  override insertChar(char: string): void {
    if (this.#insertion(char)) this.#edit(char.length, () => super.insertChar(char))
  }
  override insertText(text: string): void {
    if (this.#insertion(text)) this.#edit(text.length, () => super.insertText(text))
  }
  override newLine(): boolean { return this.#insertion("\n") && this.#edit(1, () => super.newLine()) }
  override setText(text: string): void {
    if (this.#admit(text.length, Buffer.byteLength(text))) { this.#text = null; super.setText(text); this.#text = null; this.#historyBytes = 0; this.#edits = 0 }
  }
  override replaceText(text: string): void {
    if (this.#admit(text.length, Buffer.byteLength(text))) this.#edit(text.length, () => super.replaceText(text))
  }
  override deleteSelection(): boolean { return this.#edit(0, () => super.deleteSelection()) }
  override deleteChar(): boolean { return this.#edit(0, () => super.deleteChar()) }
  override deleteCharBackward(): boolean { return this.#edit(0, () => super.deleteCharBackward()) }
  override deleteLine(): boolean { return this.#edit(0, () => super.deleteLine()) }
  override deleteToLineEnd(): boolean { return this.#edit(0, () => super.deleteToLineEnd()) }
  override deleteToLineStart(): boolean { return this.#edit(0, () => super.deleteToLineStart()) }
  override deleteWordForward(): boolean { return this.#edit(0, () => super.deleteWordForward()) }
  override deleteWordBackward(): boolean { return this.#edit(0, () => super.deleteWordBackward()) }
  override deleteRange(startLine: number, startCol: number, endLine: number, endCol: number): void {
    this.#edit(0, () => super.deleteRange(startLine, startCol, endLine, endCol))
  }
  override undo(): boolean { this.#text = null; try { return super.undo() } finally { this.#text = null } }
  override redo(): boolean { this.#text = null; try { return super.redo() } finally { this.#text = null } }
}
