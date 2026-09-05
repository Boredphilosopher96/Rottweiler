import { MAX_ATTACHMENTS_PER_MESSAGE, type Attachment } from "./protocol"
import type { ComposerDraft } from "./subagent-state"

export const MAX_CLIENT_DRAFT_BYTES = 32 * 1024 * 1024
export const MAX_CLIENT_DRAFTS = 256
export const MAX_ATTACHMENTS_PER_DRAFT = 2 * MAX_ATTACHMENTS_PER_MESSAGE
const EMPTY: ComposerDraft = { content: "", attachments: [] }
const ENTRY_BYTES = 256

/** Conservatively charge JS text plus the editable native buffer, without encoding it per keystroke. */
export function composerDraftBytes(draft: ComposerDraft): number {
  return draftBytes(draft.content.length, draft.attachments)
}

function draftBytes(contentLength: number, attachments: readonly Attachment[]): number {
  if (contentLength === 0 && attachments.length === 0) return 0
  let bytes = ENTRY_BYTES + contentLength * 6
  for (const attachment of attachments) {
    bytes += 256 + 2 * (attachment.name.length + attachment.media_type.length + (attachment.source_path?.length ?? 0))
    bytes += 2 * (attachment.data.type === "text" ? attachment.data.content.length : attachment.data.data.length)
  }
  return bytes
}

interface Entry { readonly draft: ComposerDraft; readonly bytes: number }
interface Submission { readonly scope: string; readonly entry: Entry; active: boolean }
export interface DraftSubmission {
  readonly draft: ComposerDraft
  /** Settlement consumes this reservation exactly once, independently of the active scope. */
  settle(accepted: boolean): ComposerDraft | null
}

/** Editable data is admitted or preserved; it is never an evictable display cache. */
export class ComposerDraftStore {
  readonly #drafts = new Map<string, Entry>()
  readonly #pending = new Set<Submission>()
  #bytes = 0
  constructor(readonly maximumBytes = MAX_CLIENT_DRAFT_BYTES, readonly maximumDrafts = MAX_CLIENT_DRAFTS) {
    if (!Number.isSafeInteger(maximumBytes) || maximumBytes <= 0 || !Number.isSafeInteger(maximumDrafts) || maximumDrafts <= 0) {
      throw new RangeError("invalid draft limits")
    }
  }
  get usage() { return { bytes: this.#bytes, drafts: this.#drafts.size, pending: this.#pending.size } }
  get(scope: string): ComposerDraft { return this.#drafts.get(scope)?.draft ?? EMPTY }
  entries(): readonly { readonly scope: string; readonly draft: ComposerDraft }[] {
    return [...this.#drafts].map(([scope, entry]) => ({ scope, draft: entry.draft }))
  }
  canRetainText(scope: string, codeUnits: number, attachments: readonly Attachment[]): boolean {
    if (!Number.isSafeInteger(codeUnits) || codeUnits < 0 || !this.#attachmentsFit(scope, attachments.length)) return false
    const payload = draftBytes(codeUnits, attachments)
    const bytes = payload === 0 ? 0 : payload + scope.length * 2
    const previous = this.#drafts.get(scope)
    return this.#bytes - (previous?.bytes ?? 0) + bytes <= this.maximumBytes
      && (bytes === 0 || previous !== undefined || this.#drafts.size + this.#pending.size < this.maximumDrafts)
  }
  set(scope: string, draft: ComposerDraft): boolean {
    if (!this.#attachmentsFit(scope, draft.attachments.length)) return false
    const old = this.#drafts.get(scope)
    const bytes = composerDraftBytes(draft) + (draft.content === "" && draft.attachments.length === 0 ? 0 : scope.length * 2)
    if (this.#bytes - (old?.bytes ?? 0) + bytes > this.maximumBytes
      || (bytes > 0 && old === undefined && this.#drafts.size + this.#pending.size >= this.maximumDrafts)) return false
    this.#bytes += bytes - (old?.bytes ?? 0)
    if (bytes === 0) this.#drafts.delete(scope)
    else this.#drafts.set(scope, { draft: snapshot(draft), bytes })
    return true
  }
  #attachmentsFit(scope: string, count: number): boolean {
    for (const item of this.#pending) {
      if (item.active && item.scope === scope) return count <= MAX_ATTACHMENTS_PER_MESSAGE
    }
    return count <= MAX_ATTACHMENTS_PER_DRAFT
  }
  remove(scope: string): void {
    for (const pending of this.#pending) if (pending.scope === scope) pending.active = false
    const entry = this.#drafts.get(scope)
    if (entry !== undefined) { this.#bytes -= entry.bytes; this.#drafts.delete(scope) }
  }
  clear(): void {
    for (const entry of this.#drafts.values()) this.#bytes -= entry.bytes
    this.#drafts.clear()
    // Accepted asynchronous work keeps its allocation charge until settlement.
    for (const pending of this.#pending) pending.active = false
  }

  replace(drafts: readonly { readonly scope: string; readonly draft: ComposerDraft }[]): boolean {
    if (this.#pending.size > 0) return false
    const candidate = new ComposerDraftStore(this.maximumBytes, this.maximumDrafts)
    for (const { scope, draft } of drafts) if (!candidate.set(scope, draft)) return false
    this.clear()
    for (const [scope, entry] of candidate.#drafts) this.#drafts.set(scope, entry)
    this.#bytes = candidate.#bytes
    return true
  }

  /** Transfer rather than duplicate the draft's capacity before asynchronous submission. */
  submit(scope: string): DraftSubmission | null {
    const entry = this.#drafts.get(scope)
    if (entry === undefined || entry.draft.attachments.length > MAX_ATTACHMENTS_PER_MESSAGE || this.#pending.size > 0) return null
    const submission = { scope, entry, active: true }
    this.#drafts.delete(scope)
    this.#pending.add(submission)
    let live: Submission | null = submission
    return {
      get draft() { if (live === null) throw new Error("draft submission has settled"); return live.entry.draft },
      settle: accepted => {
        const owned = live
        live = null
        return owned === null ? null : this.#settle(owned, accepted)
      },
    }
  }

  #settle(submission: Submission, accepted: boolean): ComposerDraft | null {
    if (!this.#pending.delete(submission)) return null
    this.#bytes -= submission.entry.bytes
    if (accepted || !submission.active) return null
    const current = this.#drafts.get(submission.scope)
    const restored = mergeDrafts(submission.entry.draft, current?.draft ?? EMPTY)
    // Two charged entries cover their concatenation (including its separator and metadata).
    if (!this.set(submission.scope, restored)) throw new Error("reserved submission no longer fits its draft owner")
    return this.get(submission.scope)
  }
}

function snapshot(draft: ComposerDraft): ComposerDraft {
  return { content: draft.content, attachments: draft.attachments.map(attachment =>
    Object.isFrozen(attachment) && Object.isFrozen(attachment.data) ? attachment
      : Object.freeze({ ...attachment, data: Object.freeze({ ...attachment.data }) })) }

}

export function sameAttachment(left: Attachment, right: Attachment): boolean {
  return left.name === right.name && left.media_type === right.media_type && left.source_path === right.source_path
    && left.data.type === right.data.type
    && (left.data.type === "text" ? right.data.type === "text" && left.data.content === right.data.content
      : right.data.type === "inline_base64" && left.data.data === right.data.data)
}

function mergeDrafts(submitted: ComposerDraft, current: ComposerDraft): ComposerDraft {
  const attachments = [...current.attachments]
  for (const attachment of submitted.attachments) {
    if (!attachments.some(existing => sameAttachment(existing, attachment))) attachments.push(attachment)
  }
  return { content: submitted.content === "" ? current.content : current.content === "" ? submitted.content
    : `${submitted.content}\n${current.content}`, attachments }
}
