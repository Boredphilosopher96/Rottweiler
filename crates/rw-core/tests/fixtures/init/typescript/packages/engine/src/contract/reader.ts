/** A single block candidate handed to the reader scorer (extracted in the renderer). */
export interface ReaderBlock {
  tag: string          // lowercased tag name: 'p' | 'div' | 'article' | 'section' | ...
  text: string         // visible text of the block
  linkText: string     // text inside anchors within the block
  depth: number        // DOM depth (deeper = less likely to be main content)
  role?: string        // aria role or semantic hint
}

export interface ReaderArticle {
  title: string
  byline: string | null
  blocks: ReaderBlock[]     // the chosen main-content blocks, in order
  textLength: number
  excerpt: string
  ok: boolean               // false when nothing article-like was found
}
