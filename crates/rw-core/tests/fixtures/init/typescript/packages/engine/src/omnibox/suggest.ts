import type { Suggestion } from '@shared/omnibox'

export interface SuggestInput {
  query: string
  history: Array<{ url: string; title: string; visitCount: number; lastVisit: number }>
  bookmarks: Array<{ title: string; url: string }>
  tabs: Array<{ title: string; url: string }>
  now?: number
}

const DAY = 86_400_000

/**
 * Pure omnibox ranking. Merges history (frecency: visits weighted by recency),
 * bookmarks (intentional, ranked high), and open tabs (switch-to), de-duplicated
 * by URL. UI-agnostic and injectable (`now`) so it's fully testable. The caller
 * appends a "search the web" fallback separately.
 */
export function suggest(input: SuggestInput, limit = 8): Suggestion[] {
  const q = input.query.trim().toLowerCase()
  const now = input.now ?? Date.now()
  const out = new Map<string, Suggestion>()

  const consider = (s: Suggestion): void => {
    const prev = out.get(s.url)
    if (!prev || s.score > prev.score) out.set(s.url, s)
  }

  const matches = (text: string): boolean => q === '' || text.toLowerCase().includes(q)

  for (const h of input.history) {
    if (!matches(h.url) && !matches(h.title)) continue
    const ageDays = Math.max(0, (now - h.lastVisit) / DAY)
    const recency = ageDays < 1 ? 6 : ageDays < 7 ? 3 : ageDays < 30 ? 1 : 0
    consider({ kind: 'history', title: h.title, url: h.url, score: h.visitCount * 2 + recency + 10 })
  }
  for (const b of input.bookmarks) {
    if (!b.url || (!matches(b.url) && !matches(b.title))) continue
    consider({ kind: 'bookmark', title: b.title, url: b.url, score: 60 })
  }
  for (const t of input.tabs) {
    if (!t.url || t.url === 'about:blank' || (!matches(t.url) && !matches(t.title))) continue
    consider({ kind: 'tab', title: t.title, url: t.url, score: 55 })
  }

  return [...out.values()].sort((a, b) => b.score - a.score).slice(0, limit)
}
