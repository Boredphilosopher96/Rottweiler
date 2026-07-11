import type { ReaderBlock, ReaderArticle } from '../contract/reader'

const POSITIVE_TAGS: Record<string, number> = { article: 12, main: 10, section: 4, p: 5, blockquote: 3, pre: 3, figure: 2 }
const NEGATIVE_TAGS: Record<string, number> = { nav: -20, aside: -12, footer: -14, header: -6, form: -10, button: -8 }
const NEGATIVE_ROLES = new Set(['navigation', 'complementary', 'banner', 'contentinfo', 'search'])

/** Fraction of a block's text that lives inside links — high means nav/boilerplate. */
export function linkDensity(block: ReaderBlock): number {
  const total = block.text.length
  if (!total) return 0
  return Math.min(1, block.linkText.length / total)
}

/** Heuristic content score for a single block. Higher = more article-like. */
export function scoreBlock(block: ReaderBlock): number {
  const text = block.text.trim()
  const len = text.length
  if (len === 0) return -5
  let score = 1
  score += POSITIVE_TAGS[block.tag] ?? 0
  score += NEGATIVE_TAGS[block.tag] ?? 0
  if (block.role && NEGATIVE_ROLES.has(block.role)) score -= 15
  score += Math.min(12, Math.floor(len / 100))        // length reward, capped
  score += Math.min(4, (text.match(/[,.;]/g)?.length ?? 0) / 4) // prose punctuation
  score -= linkDensity(block) * 12                     // link-density penalty
  score -= Math.max(0, block.depth - 6) * 0.6          // deep nodes less likely
  return score
}

export interface ExtractInput {
  title: string
  byline?: string | null
  blocks: ReaderBlock[]
  minChars?: number
}

/** Choose the main-content blocks and assemble a reader article. Pure + testable. */
export function extractArticle(input: ExtractInput): ReaderArticle {
  const minChars = input.minChars ?? 140
  const scored = input.blocks
    .map((b) => ({ b, s: scoreBlock(b) }))
    .filter((x) => x.s > 3 && x.b.text.trim().length >= 25 && linkDensity(x.b) < 0.5)
    .sort((a, b) => b.s - a.s)

  const chosen: ReaderBlock[] = []
  const seen = new Set<string>()
  let textLength = 0
  for (const { b } of scored) {
    const key = b.text.trim().slice(0, 80)
    if (seen.has(key)) continue
    seen.add(key)
    chosen.push(b)
    textLength += b.text.trim().length
  }
  // restore document order for the chosen set
  const order = new Map(input.blocks.map((b, i) => [b, i]))
  chosen.sort((a, b) => (order.get(a)! - order.get(b)!))

  const excerpt = chosen.map((b) => b.text.trim()).join(' ').slice(0, 280)
  return {
    title: (input.title || '').trim(),
    byline: input.byline?.trim() || null,
    blocks: chosen,
    textLength,
    excerpt,
    ok: textLength >= minChars && chosen.length > 0
  }
}
