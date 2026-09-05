import type { TranscriptContentPage, TranscriptItemId, TranscriptPage, TranscriptPosition, TranscriptView } from "../protocol"
import { TRANSCRIPT_PROJECTION_VERSION } from "../protocol"
import { parseU64 } from "../transport/types"
import { ClientCache, type CacheLease } from "./cache"
import type { HistoryReader } from "./reader"

export type HistoryCacheValue =
  | { readonly kind: "page"; readonly page: TranscriptPage }
  | { readonly kind: "document"; readonly page: TranscriptContentPage }

export interface HistoryAnchor {
  readonly id: TranscriptItemId
  readonly offset: number
}

interface SessionView {
  view: TranscriptView | null
  total: bigint
  following: boolean
  anchor: HistoryAnchor | null
  activeKey: string | null
  readonly pages: Map<string, { readonly first: bigint; readonly last: bigint }>
}

export interface HistorySnapshot {
  readonly sessionId: string | null
  readonly page: TranscriptPage | null
  readonly total: bigint
  readonly loading: boolean
  readonly error: string | null
  readonly following: boolean
  readonly selection: { readonly ordinal: bigint } | null
  readonly anchor: HistoryAnchor | null
}

const MAX_SESSION_VIEWS = 8
const MAX_SESSION_PAGES = 32
export const HISTORY_PAGE_ITEMS = 32
export const HISTORY_PAGE_BYTES = 256 * 1024

/** Current-view ownership; cached pages never become a second transcript authority. */
export class HistoryController {
  readonly cache: ClientCache<HistoryCacheValue>
  readonly #reader: HistoryReader
  readonly #changed: () => void
  readonly #sessions = new Map<string, SessionView>()
  #sessionId: string | null = null
  #active: CacheLease<HistoryCacheValue> | null = null
  #activeKey: string | null = null
  #request: AbortController | null = null
  #loading = false
  #error: string | null = null
  #selection: { readonly ordinal: bigint } | null = null
  #following = true
  #disposed = false
  #revision = 0

  constructor(reader: HistoryReader, changed: () => void, cache = new ClientCache<HistoryCacheValue>()) {
    this.#reader = reader
    this.#changed = changed
    this.cache = cache
  }

  get snapshot(): HistorySnapshot {
    const active = this.#active?.value
    return {
      sessionId: this.#sessionId,
      page: active?.kind === "page" ? active.page : null,
      total: this.#sessionId === null ? 0n : this.#sessions.get(this.#sessionId)?.total ?? 0n,
      loading: this.#loading, error: this.#error, following: this.#following, selection: this.#selection,
      anchor: this.#sessionId === null ? null : this.#sessions.get(this.#sessionId)?.anchor ?? null,
    }
  }

  async open(sessionId: string): Promise<void> {
    if (this.#disposed) return
    if (sessionId !== this.#sessionId) {
      this.#request?.abort()
      this.#request = null
      const previous = this.#active
      this.#active = null
      this.#activeKey = null
      this.#sessionId = sessionId
      this.#selection = null
      const session = this.#session(sessionId)
      this.#following = session.following
      if (session.activeKey !== null) {
        this.#active = this.cache.lease(session.activeKey)
        if (this.#active !== null) this.#activeKey = session.activeKey
      }
      this.#changed()
      previous?.release()
    }
    const session = this.#session(sessionId)
    await this.load(session.following || session.anchor === null
      ? { type: "latest" } : { type: "around", item: session.anchor.id })
  }

  async load(position: TranscriptPosition): Promise<void> {
    const sessionId = this.#sessionId
    if (this.#disposed || sessionId === null) return
    this.#request?.abort()
    const request = new AbortController()
    this.#request = request
    this.#selection = position.type === "at_ordinal" ? { ordinal: requiredU64(position.ordinal) } : null
    this.#following = position.type === "latest"
    this.#loading = true
    this.#error = null
    const session = this.#session(sessionId)
    session.following = this.#following
    if (position.type === "first" || position.type === "latest" || position.type === "at_ordinal") session.anchor = null
    this.#changed()
    let retired: CacheLease<HistoryCacheValue> | null = null
    try {
      for (; ;) {
        const result = await this.#reader.page(sessionId, {
          known_view: session.view, position, max_items: HISTORY_PAGE_ITEMS, max_bytes: HISTORY_PAGE_BYTES,
        }, request.signal)
        if (!this.#current(request, sessionId)) return
        if (result.type === "catching_up") {
          // Yield between bounded server batches; input and cancellation retain a turn.
          await new Promise<void>(resolve => setTimeout(resolve, 0))
          request.signal.throwIfAborted()
          continue
        }
        if (result.type === "ordering_changed") {
          const anchor = session.anchor?.id ?? this.snapshot.page?.items[0]?.id
          this.#invalidate(session)
          session.view = null
          position = anchor === undefined ? { type: "latest" } : { type: "around", item: anchor }
          continue
        }
        const page = result.page
        validatePage(page, sessionId)
        if (session.view?.through != null && (page.view.through == null
          || requiredU64(page.view.through) < requiredU64(session.view.through))) {
          throw new Error("history response predates the applied source prefix")
        }
        if (session.view !== null && (page.view.generation !== session.view.generation
          || page.invalidation.type !== "none")) this.#invalidate(session)
        const key = `${sessionId}:page:${++this.#revision}`
        if (!this.cache.insert(key, { kind: "page", page })) throw new Error("history cache is full with active readers")
        const lease = this.cache.lease(key)
        if (lease === null) throw new Error("admitted history page is unavailable")
        retired = this.#active
        this.#active = lease
        this.#activeKey = key
        session.activeKey = key
        session.view = page.view
        session.total = requiredU64(page.total_items)
        session.pages.set(key, { first: requiredU64(page.first_ordinal), last: requiredU64(page.first_ordinal) + BigInt(page.items.length) })
        while (session.pages.size > MAX_SESSION_PAGES) {
          const oldest = session.pages.keys().next().value
          if (oldest === undefined) break
          this.cache.remove(oldest)
          session.pages.delete(oldest)
        }
        break
      }
    } catch (error) {
      if (this.#current(request, sessionId)) this.#error = error instanceof Error ? error.message : "history read failed"
    } finally {
      if (this.#current(request, sessionId)) {
        this.#loading = false
        this.#request = null
        try { this.#changed() } finally { retired?.release() }
      } else retired?.release()
    }
  }

  /** A cache miss restores the requested region through the semantic index. */
  async seek(ordinal: bigint): Promise<void> {
    if (this.#sessionId === null) return
    const session = this.#session(this.#sessionId)
    if (session.view === null) return
    this.#following = false
    session.following = false
    session.anchor = null
    this.#selection = { ordinal }
    for (const [key, range] of session.pages) {
      if (ordinal < range.first || ordinal >= range.last) continue
      const lease = this.cache.lease(key)
      if (lease === null) { session.pages.delete(key); continue }
      this.#request?.abort()
      this.#request = null
      this.#loading = false
      const previous = this.#active
      this.#active = lease
      this.#activeKey = key
      session.activeKey = key
      try { this.#changed() } finally { previous?.release() }
      return
    }
    await this.load({ type: "at_ordinal", ordinal: ordinal.toString(), generation: session.view.generation })
  }

  refresh(): Promise<void> {
    const snapshot = this.snapshot
    const item = snapshot.anchor?.id ?? snapshot.page?.items[0]?.id
    return this.load(this.#following || item === undefined ? { type: "latest" } : { type: "around", item })
  }

  around(item: TranscriptItemId): Promise<void> { return this.load({ type: "around", item }) }

  setFollowing(following: boolean): void {
    if (this.#following === following) return
    this.#following = following
    if (this.#sessionId !== null) this.#session(this.#sessionId).following = following
    this.#changed()
  }

  setAnchor(anchor: HistoryAnchor): void {
    if (this.#sessionId !== null) this.#session(this.#sessionId).anchor = anchor
  }

  dispose(): void {
    this.#disposed = true
    this.#request?.abort()
    this.#active?.release()
    this.#active = null
    this.#activeKey = null
    this.#sessions.clear()
    this.cache.clear()
  }

  #current(request: AbortController, session: string): boolean {
    return !this.#disposed && !request.signal.aborted && this.#request === request && this.#sessionId === session
  }

  #session(id: string): SessionView {
    const existing = this.#sessions.get(id)
    if (existing !== undefined) {
      this.#sessions.delete(id)
      this.#sessions.set(id, existing)
      return existing
    }
    if (this.#sessions.size === MAX_SESSION_VIEWS) {
      const oldest = this.#sessions.entries().next().value
      if (oldest !== undefined) { this.#invalidate(oldest[1]); this.#sessions.delete(oldest[0]) }
    }
    const session: SessionView = { view: null, total: 0n, pages: new Map(), following: true, anchor: null, activeKey: null }
    this.#sessions.set(id, session)
    return session
  }

  #invalidate(session: SessionView): void {
    for (const key of session.pages.keys()) {
      this.cache.remove(key)
    }
    session.pages.clear()
    session.activeKey = null
  }
}

function requiredU64(value: string): bigint {
  const parsed = parseU64(value)
  if (parsed === null) throw new Error("history ordinal is outside u64")
  return parsed
}

function validatePage(page: TranscriptPage, session: string): void {
  const first = requiredU64(page.first_ordinal)
  const total = requiredU64(page.total_items)
  requiredU64(page.view.generation)
  const through = page.view.through == null ? null : requiredU64(page.view.through)
  if (page.view.session_id !== session || page.view.projection_version !== TRANSCRIPT_PROJECTION_VERSION
    || page.items.length > HISTORY_PAGE_ITEMS || first + BigInt(page.items.length) > total) {
    throw new Error("history page violates its session or range")
  }
  const identities = new Set<string>()
  for (const [index, item] of page.items.entries()) {
    if (requiredU64(item.ordinal) !== first + BigInt(index) || through === null
      || requiredU64(item.id) > through || requiredU64(item.revision) > through || identities.has(item.id)) {
      throw new Error("history item violates its ordinal or source prefix")
    }
    identities.add(item.id)
  }
}
