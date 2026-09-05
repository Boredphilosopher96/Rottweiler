import { CacheRead } from "./read-allocation"
import type { ComposerDraftStore, DraftSubmission, DraftTextReservation } from "../composer-drafts"
import type { TranscriptContentSource, TranscriptView } from "../protocol"
import { HistoryController, type HistoryCacheValue } from "./controller"
import type { ClientCache } from "./cache"
import type { SessionReader, SessionReadTarget } from "../session-reader"

export interface TimelineChoice {
  readonly target: SessionReadTarget
  readonly sequenceId: string
  readonly agentTurn: string
  readonly view: TranscriptView
  readonly source: TranscriptContentSource | null
  readonly preview: string
  readonly hadAttachments: boolean
}

/** Timeline navigation retains one semantic page, regardless of conversation length. */
export class TimelineController {
  readonly history: HistoryController
  constructor(readonly reader: Pick<SessionReader, "page" | "content">, cache: ClientCache<HistoryCacheValue>, changed: () => void) {
    this.history = new HistoryController(reader, changed, cache)
  }
  get choices(): readonly TimelineChoice[] {
    const page = this.history.snapshot.page
    const target = this.history.target
    if (page === null || target === null) return []
    return page.items.flatMap(item => {
      const content = item.content
      if (content.type !== "conversation" || content.role !== "user" || item.agent_turn === null) return []
      const first = content.blocks[0]
      const text = first?.type === "text" && first.body.source.selector.type === "conversation_block"
        && first.body.source.selector.index === 0 ? first.body : null
      const attachment = text !== null && /^Attached file .+ \([^\n]+\):\n/.test(text.text)
      return [{ target, sequenceId: content.source.sequence, agentTurn: item.agent_turn, view: page.view,
        source: text === null || attachment ? null : text.source, preview: attachment ? "" : text?.text ?? "",
        hadAttachments: attachment || content.omitted_blocks || content.blocks.length > 1 || text === null }]
    }).reverse()
  }
  get older(): boolean { return BigInt(this.history.snapshot.page?.first_ordinal ?? "0") > 0n }
  get newer(): boolean {
    const page = this.history.snapshot.page
    return page !== null && BigInt(page.first_ordinal) + BigInt(page.items.length) < BigInt(page.total_items)
  }
  open(target: SessionReadTarget, anchor?: string): Promise<void> {
    return anchor === undefined ? this.history.open(target)
      : this.history.restoreViewport(target, { following: false, anchor: { id: anchor, offset: 0 } })
  }
  previous(): Promise<void> {
    const first = this.history.snapshot.page?.items[0]
    return first === undefined ? Promise.resolve() : this.history.load({ type: "before", item: first.id })
  }
  next(): Promise<void> {
    const last = this.history.snapshot.page?.items.at(-1)
    return last === undefined ? Promise.resolve() : this.history.load({ type: "after", item: last.id })
  }
  dispose(): void { this.history.dispose() }
}

const READ_BYTES = 4096

/** Complete source text is admitted before joining; previews are never mutation input. */
export async function readTimelineDraft(reader: Pick<SessionReader, "page" | "content">, choice: TimelineChoice, drafts: ComposerDraftStore,
  scope: string, signal: AbortSignal, cache: ClientCache<HistoryCacheValue>): Promise<DraftSubmission> {
  let reservation: DraftTextReservation | null = null
  const chunks: string[] = []
  const incoming = new CacheRead(cache)
  try {
    signal.throwIfAborted()
    if (choice.source === null) {
      reservation = drafts.reserveText(scope, 0)
      if (reservation === null) throw new Error("Draft capacity is full; keep or submit the current draft before restoring history.")
      return reservation.finish("")
    }
    let offset = 0
    let total: number | null = null
    for (;;) {
      const page = await reader.content(choice.target, {
        view: choice.view, source: choice.source, offset, max_bytes: READ_BYTES,
      }, signal, incoming)
      signal.throwIfAborted()
      const bytes = Buffer.byteLength(page.text)
      if (JSON.stringify(page.view) !== JSON.stringify(choice.view)
        || JSON.stringify(page.source) !== JSON.stringify(choice.source)
        || page.offset !== offset || page.format !== "text" || bytes > READ_BYTES
        || !Number.isSafeInteger(page.total_bytes) || page.total_bytes < 0
        || offset + bytes > page.total_bytes || (total !== null && page.total_bytes !== total)) {
        throw new Error("History content does not match the selected source.")
      }
      if (total === null) {
        total = page.total_bytes
        reservation = drafts.reserveText(scope, total)
        if (reservation === null) throw new Error("Draft capacity is full; keep or submit the current draft before restoring history.")
      }
      const end = offset + bytes
      if ((page.next_offset === null && end !== total)
        || (page.next_offset !== null && (bytes === 0 || page.next_offset !== end || end >= total))) {
        throw new Error("History content continuation is invalid.")
      }
      // Non-final chunks must fill the requested UTF-8 window, except a split scalar.
      if (page.next_offset !== null && bytes < READ_BYTES - 3) throw new Error("History content chunk is undersized.")
      chunks.push(page.text)
      if (page.next_offset === null) return reservation!.finish(chunks.join(""))
      offset = page.next_offset
    }
  } finally {
    incoming.release()
    chunks.length = 0
    reservation?.cancel()
  }
}
