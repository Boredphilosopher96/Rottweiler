const graphemeSegmenter = new Intl.Segmenter(undefined, { granularity: "grapheme" })
const mark = /^\p{Mark}$/u
const extendedPictographic = /\p{Extended_Pictographic}/u
const regionalIndicator = /\p{Regional_Indicator}/u

export function stringCellWidth(value: string): number {
  let width = 0
  for (const { segment } of graphemeSegmenter.segment(value)) {
    width += graphemeCellWidth(segment)
  }
  return width
}

export function truncateToCells(value: string, maxCells: number, ellipsis = "…"): string {
  const limit = Number.isFinite(maxCells)
    ? Math.max(0, Math.floor(maxCells))
    : maxCells > 0 ? Number.POSITIVE_INFINITY : 0
  if (limit === Number.POSITIVE_INFINITY || stringCellWidth(value) <= limit) return value

  const ellipsisWidth = stringCellWidth(ellipsis)
  if (ellipsisWidth > limit) return ""

  const contentLimit = limit - ellipsisWidth
  let width = 0
  let result = ""
  for (const { segment } of graphemeSegmenter.segment(value)) {
    const segmentWidth = graphemeCellWidth(segment)
    if (width + segmentWidth > contentLimit) break
    result += segment
    width += segmentWidth
  }
  return result + ellipsis
}

function graphemeCellWidth(grapheme: string): number {
  let visible = false
  let wide = false
  for (const character of grapheme) {
    const codePoint = character.codePointAt(0)!
    if (isZeroWidth(codePoint, character)) continue
    visible = true
    wide ||= isWideCodePoint(codePoint)
  }
  if (!visible) return 0
  if (
    extendedPictographic.test(grapheme) ||
    regionalIndicator.test(grapheme) ||
    grapheme.includes("\ufe0f") ||
    grapheme.includes("\u20e3")
  ) return 2
  return wide ? 2 : 1
}

function isZeroWidth(codePoint: number, character: string): boolean {
  return (
    codePoint <= 0x1f ||
    (codePoint >= 0x7f && codePoint <= 0x9f) ||
    mark.test(character) ||
    (codePoint >= 0xfe00 && codePoint <= 0xfe0f) ||
    (codePoint >= 0xe0100 && codePoint <= 0xe01ef) ||
    (codePoint >= 0x200b && codePoint <= 0x200f) ||
    codePoint === 0xfeff
  )
}

// Compact Unicode block coverage matches common terminal wide/fullwidth and emoji behavior; ambiguous-width and newly assigned code points can still vary by terminal.
function isWideCodePoint(codePoint: number): boolean {
  return codePoint >= 0x1100 && (
    codePoint <= 0x115f ||
    codePoint === 0x2329 ||
    codePoint === 0x232a ||
    (codePoint >= 0x2e80 && codePoint <= 0x303e) ||
    (codePoint >= 0x3040 && codePoint <= 0xa4cf) ||
    (codePoint >= 0xac00 && codePoint <= 0xd7a3) ||
    (codePoint >= 0xf900 && codePoint <= 0xfaff) ||
    (codePoint >= 0xfe10 && codePoint <= 0xfe19) ||
    (codePoint >= 0xfe30 && codePoint <= 0xfe6f) ||
    (codePoint >= 0xff00 && codePoint <= 0xff60) ||
    (codePoint >= 0xffe0 && codePoint <= 0xffe6) ||
    (codePoint >= 0x1b000 && codePoint <= 0x1b2ff) ||
    (codePoint >= 0x1f200 && codePoint <= 0x1f251) ||
    (codePoint >= 0x20000 && codePoint <= 0x3fffd)
  )
}
