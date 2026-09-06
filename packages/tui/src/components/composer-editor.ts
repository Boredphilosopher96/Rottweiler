import { TextareaRenderable, resolveRenderLib, type KeyEvent, type RenderContext, type TextareaOptions } from "@opentui/core"

export const MAX_COMPOSER_UNDO_BYTES = 4 * 1024 * 1024

/** Bound both native text growth and edit-history retention at their mutation owner. */
export class ComposerEditorRenderable extends TextareaRenderable {
  #canRetain: ((codeUnits: number) => boolean) | undefined
  #historyBytes = 0
  #editing = false
  readonly #maximumHistoryBytes: number
  constructor(ctx: RenderContext, options: TextareaOptions, canRetain: (codeUnits: number) => boolean,
    maximumHistoryBytes = MAX_COMPOSER_UNDO_BYTES) {
    super(ctx, options)
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
  override get plainText(): string { return this.#readText(false) }
  override getSelectedText(): string { return this.#readText(true) }
  get historyCharge(): number { return this.#historyBytes }
  #admit(length: number): boolean { return this.#canRetain?.(length) ?? true }
  #insertion(text: string): boolean {
    return this.#admit(this.plainText.length - this.getSelectedText().length + text.length)
  }
  #checkpoint(): void {
    const text = this.plainText
    const cursor = this.cursorOffset
    const selection = this.getSelection()
    // setText releases the native add-buffer as well as its undo records.
    super.setText(text)
    this.cursorOffset = cursor
    if (selection !== null) this.setSelection(selection.start, selection.end)
    this.#historyBytes = 0
  }
  #edit<T>(insertedLength: number, action: () => T): T {
    if (this.#editing) return action()
    const charge = 512 + 6 * (this.plainText.length + insertedLength)
    if (this.#historyBytes + charge > this.#maximumHistoryBytes) this.#checkpoint()
    this.#editing = true
    try { return action() }
    finally {
      this.#editing = false
      this.#historyBytes += charge
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
    if (this.#admit(text.length)) { super.setText(text); this.#historyBytes = 0 }
  }
  override replaceText(text: string): void {
    if (this.#admit(text.length)) this.#edit(text.length, () => super.replaceText(text))
  }
  override handleKeyPress(key: KeyEvent): boolean {
    // Deletion actions may operate directly on the native edit buffer. Insertion
    // methods above own typed text and paste; navigation/undo do not add history.
    const deletion = key.name === "backspace" || key.name === "delete"
      || (key.ctrl && ["k", "u", "w", "d", "h"].includes(key.name))
    return deletion ? this.#edit(0, () => super.handleKeyPress(key)) : super.handleKeyPress(key)
  }
}
