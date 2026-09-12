import { DISPLAY_TRUNCATION_MARKER, type ToolOutputBuffer } from "./display-buffer"

export interface ToolOutputText { readonly plain: string; readonly labeled: string }

/** One visible output reader owns complete strings; card previews never instantiate this owner. */
export class ToolOutputReader {
  #buffer: ToolOutputBuffer | null = null
  #plain = ""
  #labeled = ""
  #view: ToolOutputText | null = null
  #visitedChunks = 0
  get visitedChunks(): number { return this.#visitedChunks }
  get retainedCodeUnits(): number { return this.#plain.length + this.#labeled.length }

  read(buffer: ToolOutputBuffer): ToolOutputText {
    if (buffer === this.#buffer && this.#view !== null) return this.#view
    let chunks = buffer.appendedAfter(this.#buffer)
    let plain = this.#plain, labeled = this.#labeled
    if (chunks === null) { chunks = buffer.appendedAfter(null)!; plain = ""; labeled = "" }
    this.#visitedChunks += chunks.length
    for (const value of chunks) {
      plain += value.chunk
      labeled += `${labeled === "" ? "" : "\n"}${value.stream === "stderr" ? "Error output" : "Output"}\n${value.chunk.trimEnd()}`
    }
    const view = buffer.truncated ? { plain: `${plain}\n${DISPLAY_TRUNCATION_MARKER}`, labeled: `${labeled}\n${DISPLAY_TRUNCATION_MARKER}` }
      : { plain, labeled }
    // An older immutable prefix cannot roll the reader's forward cursor backwards.
    if (this.#buffer === null || !buffer.sameStream(this.#buffer) || buffer.count >= this.#buffer.count) {
      this.#buffer = buffer; this.#plain = plain; this.#labeled = labeled; this.#view = view
    }
    return view
  }

  clear(): void { this.#buffer = null; this.#plain = ""; this.#labeled = ""; this.#view = null }
}
