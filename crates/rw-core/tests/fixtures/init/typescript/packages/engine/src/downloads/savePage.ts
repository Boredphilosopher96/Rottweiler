import type { SavePageFormat } from '../contract/savePage'

const BAD = /[\x00-\x1f<>:"/\\|?*]+/g
const RESERVED = /^(con|prn|aux|nul|com[1-9]|lpt[1-9])$/i

function hostPathStem(url: string): string {
  try {
    const u = new URL(url)
    const path = u.pathname.replace(/\/+$/, '')
    const last = path.split('/').filter(Boolean).pop()
    const stem = last ? last.replace(/\.[a-z0-9]{1,8}$/i, '') : u.hostname.replace(/^www\./, '')
    return stem || u.hostname || 'page'
  } catch { return 'page' }
}

/** Turn a page title (+ url fallback) into a safe filename with the right extension. */
export function savePageFilename(title: string, url: string, format: SavePageFormat): string {
  const ext = format === 'mhtml' ? '.mhtml' : '.html'
  let base = (title || '').trim().replace(BAD, ' ').replace(/\s+/g, ' ').trim()
  if (!base) base = hostPathStem(url)
  base = base.slice(0, 120).trim().replace(/[. ]+$/, '')
  if (!base || RESERVED.test(base)) base = 'page'
  return base + ext
}

/** Map our format to Electron webContents.savePage()'s saveType string. */
export function electronSaveType(format: SavePageFormat): 'HTMLComplete' | 'HTMLOnly' | 'MHTML' {
  if (format === 'mhtml') return 'MHTML'
  if (format === 'html-only') return 'HTMLOnly'
  return 'HTMLComplete'
}
