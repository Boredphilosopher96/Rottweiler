import { expect, spyOn, test } from "bun:test"

import { currentMemoryUsage, currentResidentBytes } from "../src/process-memory"
import { TestProcessScope } from "./support/owned-process"

test("resident memory sampling agrees with a fresh pinned runtime's byte measurement", async () => {
  const source = new URL("../src/process-memory.ts", import.meta.url).href
  const owner = await TestProcessScope.create("rw-memory-sampling-")
  try {
  const child = await owner.run([process.execPath, "-e", `
    import { observedResidentBytes, currentResidentBytes } from ${JSON.stringify(source)}
    const allocation = new Uint8Array(16 * 1024 * 1024)
    allocation.fill(1)
    process.stdout.write(JSON.stringify({
      observed: observedResidentBytes(), rss: currentResidentBytes(),
      retained: allocation[0],
    }))
  `], { timeoutMs: 10_000 })
  const output = child.stdout
  expect({ code: child.code, error: child.stderr }).toEqual({ code: 0, error: "" })
  const result: unknown = JSON.parse(output)
  if (result === null || typeof result !== "object" || !("observed" in result) ||
      !("rss" in result) || typeof result.observed !== "number" || typeof result.rss !== "number") {
    throw new Error("resident memory probe returned invalid observations")
  }
  expect(result.rss).toBeGreaterThan(16 * 1024 * 1024)
  expect(result.observed).toBeGreaterThanOrEqual(result.rss * 0.5)
  expect(result.observed).toBeLessThan(result.rss * 8)
  } finally { await owner.close() }
}, 40_000)


test("memory observations retry only interrupted memoryUsage syscalls and never reuse a prior sample", () => {
  const interrupted = Object.assign(new Error("interrupted"), { errno: 4, syscall: "memoryUsage" })
  let attempts = 0
  const rss = spyOn(process.memoryUsage, "rss").mockImplementation(() => {
    if (++attempts < 3) throw interrupted
    return 123456
  })
  try { expect(currentResidentBytes()).toBe(123456); expect(attempts).toBe(3) }
  finally { rss.mockRestore() }
  for (const error of [interrupted, Object.assign(new Error("denied"), { errno: 13, syscall: "memoryUsage" }),
    Object.assign(new Error("other syscall"), { errno: 4, syscall: "other" })]) {
    let calls = 0
    const read = Object.assign(() => { calls++; throw error }, { rss: process.memoryUsage.rss })
    const sample = spyOn(process, "memoryUsage").mockImplementation(read)
    try {
      expect(() => currentMemoryUsage()).toThrow(error)
      expect(calls).toBe(error === interrupted ? 3 : 1)
    } finally { sample.mockRestore() }
  }
})
