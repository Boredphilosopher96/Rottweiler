import { readToolSurface } from "./tool-surface"
import type { UiSurfaceModel } from "../ui/presentation"
import type { TranscriptContentPage, TranscriptContentSource, TranscriptView } from "../protocol"
import type { CacheLease, ClientCache } from "./cache"
import type { HistoryCacheValue } from "./controller"
import type { HistoryReader } from "./reader"

const CHUNK_BYTES = 4096
const MAX_DOCUMENT_BYTES = 16 * 1024 * 1024
const MAX_CHUNKS = Math.ceil(MAX_DOCUMENT_BYTES / (CHUNK_BYTES - 3)) + 1

export interface DocumentSnapshot {
  readonly open: boolean
  readonly page: TranscriptContentPage | null
  readonly surface: UiSurfaceModel | null
  readonly loading: boolean
  readonly error: string | null
  readonly previous: boolean
}

/** One paged document reader shares the transcript cache and never assembles full content. */
export class DocumentController {
  readonly #reader: HistoryReader
  readonly #cache: ClientCache<HistoryCacheValue>
  readonly #changed: (snapshot: DocumentSnapshot) => void
  #selection: { readonly view: TranscriptView; readonly source: TranscriptContentSource; readonly key: string } | null = null
  #request: AbortController | null = null
  #active: CacheLease<HistoryCacheValue> | null = null
  #offsets = [0]
  #index = 0
  #loading = false
  #error: string | null = null

  constructor(reader: HistoryReader, cache: ClientCache<HistoryCacheValue>, changed: (snapshot: DocumentSnapshot) => void) {
    this.#reader = reader
    this.#cache = cache
    this.#changed = changed
  }

  get snapshot(): DocumentSnapshot {
    const value = this.#active?.value
    return {
      open: this.#selection !== null, page: value?.kind === "document" ? value.page : null,
      surface: value?.kind === "surface" ? value.surface : null,
      loading: this.#loading, error: this.#error, previous: this.#index > 0
    }
  }

  open(view: TranscriptView, source: TranscriptContentSource): Promise<void> {
    this.close()
    this.#selection = { view, source, key: JSON.stringify([view, source]) }
    return this.#load(0, 0)
  }

  next(): Promise<void> {
    const offset = this.snapshot.page?.next_offset
    return offset == null || this.#loading ? Promise.resolve() : this.#load(offset, this.#index + 1)
  }

  previous(): Promise<void> {
    const offset = this.#offsets[this.#index - 1]
    return offset === undefined || this.#loading ? Promise.resolve() : this.#load(offset, this.#index - 1)
  }

  close(): void {
    this.#request?.abort()
    this.#request = null
    const previous = this.#active
    this.#active = null
    this.#selection = null
    this.#offsets = [0]
    this.#index = 0
    this.#loading = false
    this.#error = null
    try { this.#changed(this.snapshot) } finally { previous?.release() }
  }

  async #load(offset: number, index: number): Promise<void> {
    const selection = this.#selection
    if (selection === null) return
    this.#request?.abort()
    const request = new AbortController()
    this.#request = request
    this.#loading = true
    this.#error = null
    this.#changed(this.snapshot)
    let retired: CacheLease<HistoryCacheValue> | null = null
    try {
      if (index >= MAX_CHUNKS) throw new Error("document exceeds the bounded content index")
      const key = `document:${selection.key}:${offset}`
      let lease = this.#cache.lease(key)
      if (lease === null && selection.source.selector.type === "tool_presentation") {
        lease = await readToolSurface(this.#reader, this.#cache, key, selection.view, selection.source, request.signal)
        if (this.#request !== request || request.signal.aborted) { lease.release(); return }
      }
      if (lease === null) {
        const page = await this.#reader.content(selection.view.session_id, {
          view: selection.view, source: selection.source, offset, max_bytes: CHUNK_BYTES,
        }, request.signal)
        if (this.#request !== request || request.signal.aborted) return
        const bytes = Buffer.byteLength(page.text)
        if (JSON.stringify([page.view, page.source]) !== selection.key || page.offset !== offset
          || bytes > CHUNK_BYTES || page.total_bytes > MAX_DOCUMENT_BYTES
          || offset + bytes > page.total_bytes
          || (page.next_offset === null ? offset + bytes !== page.total_bytes
            : page.next_offset !== offset + bytes || bytes === 0)) {
          throw new Error("document reply violates its source or byte range")
        }
        if (!this.#cache.insert(key, { kind: "document", page })) throw new Error("content cache is full with active readers")
        lease = this.#cache.lease(key)
      }
      if (lease === null || (lease.value.kind !== "document" && lease.value.kind !== "surface")) {
        lease?.release()
        throw new Error("admitted content is unavailable")
      }
      retired = this.#active
      this.#active = lease
      this.#index = index
      this.#offsets[index] = offset
    } catch (error) {
      if (this.#request === request && !request.signal.aborted) {
        this.#error = error instanceof Error ? error.message : "content read failed"
      }
    } finally {
      if (this.#request === request) {
        this.#request = null
        this.#loading = false
        try { this.#changed(this.snapshot) } finally { retired?.release() }
      } else retired?.release()
    }
  }
}
