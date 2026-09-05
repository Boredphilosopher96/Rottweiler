import { expect, test } from "bun:test"
import { jsonEncodedBytes } from "../src/json-size"

test("allocation-free JSON size matches encoding for escapes, all UTF-16 classes and typed envelopes", () => {
  const strings = ["", "ascii", "\"\\\t\n\b\f\r\u0000", "λ雪🍋", "\ud800", "\udfff", "\ud800x\udc00"]
  for (let unit = 0; unit < 65536; unit += 97) strings.push(String.fromCharCode(unit, unit + 1, unit + 2))
  for (const content of strings) {
    const value = { content, attachments: [{ name: "a.txt", media_type: "text/plain", source_path: undefined, data: { type: "text", content } }], null: null, boolean: true, nums: [1, -0, 1e-7, Infinity] }
    const expected = Buffer.byteLength(JSON.stringify(value))
    expect(jsonEncodedBytes(value, expected)).toBe(expected)
    expect(jsonEncodedBytes(value, expected - 1)).toBe(expected)
  }
})

test("size traversal stops at its byte and depth bounds", () => {
  expect(jsonEncodedBytes("x".repeat(1_000_000), 100)).toBe(101)
  const value: Record<string, unknown> = {}; value.self = value
  expect(jsonEncodedBytes(value, 1000)).toBe(1001)
})
