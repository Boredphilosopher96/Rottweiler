import { randomUUID } from 'node:crypto'
import type { BookmarkNode } from '@shared/bookmarks'

/**
 * Parse the Netscape bookmark HTML format that Chrome/Edge/Firefox/Safari all
 * export. Returns a flat BookmarkNode[] parented under `parentId`, preserving
 * folder nesting. Deliberately tolerant: it scans for the meaningful tags rather
 * than trying to be a strict HTML parser.
 */
export function importNetscape(html: string, parentId: string): BookmarkNode[] {
  const out: BookmarkNode[] = []
  const stack: string[] = [parentId]
  const now = Date.now()

  // Tokenize the tags we care about, in document order.
  const tokenRe = /<DT>\s*<H3[^>]*>(.*?)<\/H3>|<DT>\s*<A\s+([^>]*?)>(.*?)<\/A>|<\/DL>/gis
  let m: RegExpExecArray | null

  while ((m = tokenRe.exec(html)) !== null) {
    if (m[0].toUpperCase().startsWith('</DL')) {
      if (stack.length > 1) stack.pop()
      continue
    }
    const parent = stack[stack.length - 1]

    if (m[1] !== undefined) {
      // folder open
      const id = randomUUID()
      out.push({
        id,
        type: 'folder',
        title: decode(m[1].trim()) || 'Folder',
        parentId: parent,
        createdAt: now
      })
      stack.push(id)
    } else if (m[2] !== undefined) {
      // anchor / bookmark
      const attrs = m[2]
      const href = /href\s*=\s*"([^"]*)"/i.exec(attrs)?.[1]
      if (!href) continue
      out.push({
        id: randomUUID(),
        type: 'bookmark',
        title: decode((m[3] ?? '').trim()) || href,
        url: href,
        parentId: parent,
        createdAt: now
      })
    }
  }

  return out
}

function decode(s: string): string {
  return s
    .replace(/&amp;/g, '&')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
}
