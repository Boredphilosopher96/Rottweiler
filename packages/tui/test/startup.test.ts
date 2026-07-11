import { describe, expect, test } from "bun:test"

import { writeStartupSplash } from "../src/startup"

describe("production startup splash", () => {
  test("paints a plain marker for redirected output", () => {
    const writes: string[] = []
    writeStartupSplash({ isTTY: false, write: (content) => writes.push(content) })
    expect(writes).toEqual(["Rottweiler\n"])
  })

  test("paints a complete ANSI terminal frame before OpenTUI loads", () => {
    const writes: string[] = []
    writeStartupSplash({ isTTY: true, write: (content) => writes.push(content) })
    expect(writes).toHaveLength(1)
    expect(writes[0]).toStartWith("\u001b[2J\u001b[H")
    expect(writes[0]).toContain("Rottweiler")
    expect(writes[0]).toContain("waking the engine")
  })
})
