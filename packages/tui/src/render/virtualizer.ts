import type { TranscriptEntry } from "../state"
import { turnMarkdown } from "./format"

export interface VirtualWindow {
  readonly start: number
  readonly end: number
  readonly topSpacer: number
  readonly bottomSpacer: number
  readonly totalHeight: number
}

export class TranscriptVirtualizer {
  readonly #overscan: number
  #width = 0
  #keys: string[] = []
  #heights: number[] = []
  #offsets: number[] = [0]

  constructor(overscan = 3) {
    if (!Number.isSafeInteger(overscan) || overscan < 0) {
      throw new RangeError("transcript overscan must be a non-negative integer")
    }
    this.#overscan = overscan
  }

  update(
    entries: readonly TranscriptEntry[],
    width: number,
    extraRows: (entry: TranscriptEntry) => number = () => 0,
    measuredRows: ReadonlyMap<string, number> = new Map(),
    layoutRevision = "0",
  ): void {
    const normalizedWidth = Math.max(8, Math.floor(width))
    const sameWidth = normalizedWidth === this.#width
    const nextKeys = entries.map((entry) =>
      entryLayoutKey(entry, normalizedWidth, extraRows(entry), layoutRevision)
    )
    let stable = 0
    if (sameWidth) {
      const limit = Math.min(nextKeys.length, this.#keys.length)
      while (stable < limit && nextKeys[stable] === this.#keys[stable]) {
        stable += 1
      }
    }
    const heights = sameWidth ? this.#heights.slice(0, stable) : []
    for (let index = heights.length; index < entries.length; index += 1) {
      const entry = entries[index]!
      const extra = extraRows(entry)
      const layoutKey = entryLayoutKey(entry, normalizedWidth, extra, layoutRevision)
      heights.push(
        measuredRows.get(layoutKey) ?? estimateEntryHeight(entry, normalizedWidth) + extra,
      )
    }
    const offsets = new Array<number>(heights.length + 1)
    offsets[0] = 0
    for (let index = 0; index < heights.length; index += 1) {
      offsets[index + 1] = offsets[index]! + heights[index]!
    }
    this.#width = normalizedWidth
    this.#keys = nextKeys
    this.#heights = heights
    this.#offsets = offsets
  }

  window(scrollTop: number, viewportHeight: number): VirtualWindow {
    const totalHeight = this.#offsets.at(-1) ?? 0
    if (this.#heights.length === 0) {
      return { start: 0, end: 0, topSpacer: 0, bottomSpacer: 0, totalHeight }
    }
    const top = Math.max(0, Math.min(totalHeight, Math.floor(scrollTop)))
    const bottom = Math.max(top, top + Math.max(1, Math.floor(viewportHeight)))
    const first = findContainingOffset(this.#offsets, top)
    const last = Math.min(this.#heights.length, findContainingOffset(this.#offsets, bottom) + 1)
    const start = Math.max(0, first - this.#overscan)
    const end = Math.min(this.#heights.length, last + this.#overscan)
    return {
      start,
      end,
      topSpacer: this.#offsets[start] ?? 0,
      bottomSpacer: Math.max(0, totalHeight - (this.#offsets[end] ?? totalHeight)),
      totalHeight,
    }
  }

  heightAt(index: number): number {
    return this.#heights[index] ?? 0
  }

  offsetAt(index: number): number {
    return this.#offsets[Math.max(0, Math.min(index, this.#heights.length))] ?? 0
  }

  anchor(scrollTop: number): { readonly index: number; readonly offsetWithin: number } | null {
    if (this.#heights.length === 0) return null
    const totalHeight = this.#offsets.at(-1) ?? 0
    const top = Math.max(0, Math.min(totalHeight, Math.floor(scrollTop)))
    const index = findContainingOffset(this.#offsets, top)
    return { index, offsetWithin: Math.max(0, top - this.offsetAt(index)) }
  }

  get totalHeight(): number {
    return this.#offsets.at(-1) ?? 0
  }
}

export function entryKey(entry: TranscriptEntry): string {
  return `${entry.sequenceId}:${entry.agentTurn}:${entry.turn.role}`
}

export function entryLayoutKey(
  entry: TranscriptEntry,
  width: number,
  extraRows: number,
  layoutRevision: string,
): string {
  return `${entryKey(entry)}:${Math.max(8, Math.floor(width))}:${extraRows}:${layoutRevision}`
}

export function estimateEntryHeight(entry: TranscriptEntry, width: number): number {
  const contentWidth = Math.max(8, width - 4)
  const markdown = turnMarkdown(entry.turn)
  if (entry.turn.role === "tool" && markdown === "") return 0
  const lines = markdown.split("\n")
  const contentRows = lines.reduce(
    (rows, line) => rows + Math.max(1, Math.ceil(Math.max(1, line.length) / contentWidth)),
    0,
  )
  return Math.max(3, contentRows + 2)
}

function findContainingOffset(offsets: readonly number[], target: number): number {
  let low = 0
  let high = Math.max(0, offsets.length - 2)
  while (low < high) {
    const middle = Math.floor((low + high) / 2)
    if ((offsets[middle + 1] ?? Number.POSITIVE_INFINITY) <= target) {
      low = middle + 1
    } else {
      high = middle
    }
  }
  return low
}
