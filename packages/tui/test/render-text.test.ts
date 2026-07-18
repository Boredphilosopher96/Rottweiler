import { describe, expect, test } from "bun:test"

import { stringCellWidth, truncateToCells } from "../src/render"

describe("terminal cell-aware text", () => {
  test("measures strings in terminal cells", () => {
    expect(stringCellWidth("ascii")).toBe(5)
    expect(stringCellWidth("界")).toBe(2)
    expect(stringCellWidth("👨‍👩‍👧‍👦")).toBe(2)
    expect(stringCellWidth("e\u0301")).toBe(1)
  })

  test("passes ASCII through when it fits", () => {
    expect(truncateToCells("plain text", 10)).toBe("plain text")
  })

  test("truncates CJK only at a cell boundary", () => {
    expect(truncateToCells("界界界", 5)).toBe("界界…")
  })

  test("keeps or drops a ZWJ emoji family as a whole", () => {
    const value = "A👨‍👩‍👧‍👦B"
    expect(truncateToCells(value, 4)).toBe(value)
    expect(truncateToCells(value, 3)).toBe("A…")
  })

  test("never splits a combining-mark sequence", () => {
    expect(truncateToCells("e\u0301xy", 2)).toBe("e\u0301…")
  })

  test("reserves the configured ellipsis width", () => {
    expect(truncateToCells("abcdef", 5, "..")).toBe("abc..")
  })
})
