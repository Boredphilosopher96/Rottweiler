import { MAX_PENDING_TOOL_INVOCATIONS, MAX_TURN_CITATIONS, TRANSCRIPT_TAIL_PAGE_BYTES, TRANSCRIPT_TAIL_PAGE_ITEMS, TRANSCRIPT_PROJECTION_VERSION } from "../../../../protocol/types"
import type { TranscriptTailIdentity, TranscriptTailPage, TranscriptTailPart, TranscriptTailRead, TranscriptTailResult } from "../protocol"
import type { ReplyAllocation } from "../transport/reply-allocation"
import { EngineProtocolError } from "../transport/errors"
import type { SessionReadTarget } from "../session-reader"
import { CacheRead } from "./read-allocation"
import type { CacheLease, ClientCache } from "./cache"
import type { HistoryCacheValue } from "./controller"

export type TailRead = (target: SessionReadTarget, read: TranscriptTailRead, signal: AbortSignal, allocation: ReplyAllocation) => Promise<TranscriptTailResult>
export class TailChanged extends Error {}

export function sameTailIdentity(left: TranscriptTailIdentity, right: TranscriptTailIdentity): boolean {
  return left.generation === right.generation && left.turn_started === right.turn_started
    && left.response_epoch === right.response_epoch && left.tools_epoch === right.tools_epoch
}

/** Mounted state retains the exact admitted pages; release severs every payload reference. */
export class LiveTailSnapshot {
  #pages: CacheLease<HistoryCacheValue>[] | null
  constructor(pages: CacheLease<HistoryCacheValue>[]) { this.#pages = pages }
  get pages(): readonly TranscriptTailPage[] {
    if (this.#pages === null) throw new Error("live tail snapshot is released")
    return this.#pages.map(lease => {
      const value = lease.value
      if (value.kind !== "tail_page") throw new Error("live tail lease has an invalid value")
      return value.page
    })
  }
  release(): void {
    const pages = this.#pages
    this.#pages = null
    for (const page of pages ?? []) page.release()
  }
}

/** Sequential component reads share one epoch; an advancing prefix never implies immutable slots. */
export async function collectLiveTail(
  read: TailRead, cache: ClientCache<HistoryCacheValue>, target: SessionReadTarget, signal: AbortSignal,
): Promise<LiveTailSnapshot> {
  const leases: CacheLease<HistoryCacheValue>[] = []
  const namespace = crypto.randomUUID()
  let identity: TranscriptTailIdentity | null = null
  let through: bigint | null = null
  const part = async (request: TranscriptTailPart): Promise<TranscriptTailPage> => {
    signal.throwIfAborted()
    const allocation = new CacheRead(cache)
    try {
      const result = await read(target, { expected: identity, part: request, max_items: TRANSCRIPT_TAIL_PAGE_ITEMS, max_bytes: TRANSCRIPT_TAIL_PAGE_BYTES }, signal, allocation)
      signal.throwIfAborted()
      if (result.type !== "ready") throw new TailChanged(result.type === "changed" ? "live tail changed during recovery" : "live tail projection is catching up")
      const page = result.page
      if (page.view.session_id !== target.sessionId || page.view.projection_version !== TRANSCRIPT_PROJECTION_VERSION
        || page.view.generation !== page.identity.generation || (identity !== null && !sameTailIdentity(identity, page.identity))
        || page.content.type !== request.type) throw new EngineProtocolError("live tail reply does not match its source request")
      const nextThrough = page.view.through === null ? null : BigInt(page.view.through)
      if (through !== null && (nextThrough === null || nextThrough < through)) throw new TailChanged("live tail prefix moved backwards")
      identity = page.identity
      through = nextThrough
      const key = `${namespace}:${leases.length}`
      const lease = allocation.commit(key, { kind: "tail_page", page })
      leases.push(lease)
      // Bootstrap pages are transient pins, not an independently reusable history cache.
      cache.remove(key)
      return page
    } finally { allocation.release() }
  }
  try {
    await part({ type: "text" })
    await part({ type: "thinking" })
    for (const type of ["citations", "tools"] as const) {
      const maximum = type === "citations" ? MAX_TURN_CITATIONS : MAX_PENDING_TOOL_INVOCATIONS
      let offset = 0
      let count = 0
      while (true) {
        const page = await part({ type, offset })
        const content = page.content
        if (content.type !== type || content.offset !== offset) throw new EngineProtocolError("live tail page changed its requested offset")
        count += content.items.length
        if (count > maximum || content.items.length > TRANSCRIPT_TAIL_PAGE_ITEMS) throw new EngineProtocolError("live tail page exceeds its source-owned item allowance")
        const next = content.next_offset
        if (next === null) break
        if (!Number.isInteger(next) || next <= offset || next >= maximum) throw new EngineProtocolError("live tail page has a non-progressing cursor")
        offset = next
      }
    }
    return new LiveTailSnapshot(leases)
  } catch (error) {
    for (const lease of leases) lease.release()
    throw error
  }
}
