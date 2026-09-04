import { expect, test } from "bun:test"

test("resident memory sampling agrees with a fresh pinned runtime's byte measurement", async () => {
  const source = new URL("../src/process-memory.ts", import.meta.url).href
  const child = Bun.spawn([process.execPath, "-e", `
    import { observedResidentBytes } from ${JSON.stringify(source)}
    const allocation = new Uint8Array(16 * 1024 * 1024)
    allocation.fill(1)
    process.stdout.write(JSON.stringify({
      observed: observedResidentBytes(), rss: process.memoryUsage.rss(),
      retained: allocation[0],
    }))
  `], { stdout: "pipe", stderr: "pipe" })
  const output = await new Response(child.stdout).text()
  expect(await child.exited).toBe(0)
  const result: unknown = JSON.parse(output)
  if (result === null || typeof result !== "object" || !("observed" in result) ||
      !("rss" in result) || typeof result.observed !== "number" || typeof result.rss !== "number") {
    throw new Error("resident memory probe returned invalid observations")
  }
  expect(result.rss).toBeGreaterThan(16 * 1024 * 1024)
  expect(result.observed).toBeGreaterThanOrEqual(result.rss * 0.5)
  expect(result.observed).toBeLessThan(result.rss * 8)
})
