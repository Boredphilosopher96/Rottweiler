import { prepareUiSurface } from "../ui/presentation"
import validatePresentation from "../../../../protocol/ui-presentation-validator.js"
import { MAX_UI_SURFACE_BYTES, type TranscriptContentSource, type TranscriptView } from "../protocol"
import type { ClientCache, CacheLease } from "./cache"
import type { HistoryCacheValue } from "./controller"
import type { SessionReader, SessionReadTarget } from "../session-reader"

// Charge collection, UTF-16 decoding and the JSON object graph before reading.
// Even one container per source byte fits this conservative object/string allowance.
const DECODE_CHARGE = MAX_UI_SURFACE_BYTES * 96
const CHUNK_BYTES = 4096

export async function readToolSurface(
  reader: Pick<SessionReader, "page" | "content">, cache: ClientCache<HistoryCacheValue>, target: SessionReadTarget, key: string,
  view: TranscriptView, source: TranscriptContentSource, signal: AbortSignal,
): Promise<CacheLease<HistoryCacheValue>> {
  if (source.selector.type !== "tool_presentation") throw new Error("tool surface source is required")
  const reserved = cache.reserve(DECODE_CHARGE)
  if (reserved === null) throw new Error("surface cache is full with active readers")
  try {
    signal.throwIfAborted()
    const sourceKey = JSON.stringify([view, source])
    const bytes = new Uint8Array(MAX_UI_SURFACE_BYTES)
    const encoder = new TextEncoder()
    let offset = 0
    let total: number | null = null
    for (let pageIndex = 0; ; pageIndex++) {
      if (pageIndex > Math.ceil(MAX_UI_SURFACE_BYTES / (CHUNK_BYTES - 3))) throw new Error("tool surface has too many content pages")
      const page = await reader.content(target, { view, source, offset, max_bytes: CHUNK_BYTES }, signal)
      signal.throwIfAborted()
      const length = Buffer.byteLength(page.text)
      if (JSON.stringify([page.view, page.source]) !== sourceKey || page.offset !== offset
        || page.format !== "json" || length > CHUNK_BYTES || page.total_bytes > MAX_UI_SURFACE_BYTES
        || (total !== null && total !== page.total_bytes) || offset + length > page.total_bytes
        || (page.next_offset === null ? offset + length !== page.total_bytes
          : page.next_offset !== offset + length || length === 0)) {
        throw new Error("tool surface reply violates its source or byte range")
      }
      total = page.total_bytes
      const encoded = encoder.encodeInto(page.text, bytes.subarray(offset))
      if (encoded.written !== length) throw new Error("tool surface source exceeds reserved bytes")
      offset += length
      if (page.next_offset === null) break
    }
    const value: unknown = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes.subarray(0, offset)))
    if (!validatePresentation(value) || value.descriptor.surface.surface !== "tool") {
      throw new Error("tool surface source violates its presentation contract")
    }
    signal.throwIfAborted()
    return reserved.commit(key, { kind: "surface", surface: prepareUiSurface(value) })
  } finally { reserved.release() }
}
