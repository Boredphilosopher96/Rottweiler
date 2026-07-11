/** Formats Sora can serialize the current page to. */
export type SavePageFormat = 'html' | 'mhtml' | 'html-only'

export interface SavePageResult {
  ok: boolean
  path?: string
  format: SavePageFormat
  error?: string
}
