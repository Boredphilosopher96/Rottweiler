import { expect, test } from "bun:test"
import { mkdtempSync, readFileSync, rmSync } from "node:fs"
import { join } from "node:path"
import { MEMORY_LOAD } from "../src/diagnostics/memory-fixture"

test("actual App/HTTP ownership workload retires each generation and restores process handoff", async () => {
  const directory = mkdtempSync("/tmp/rw-client-memory-test-")
  const source = new URL("../src/diagnostics/memory-probe.ts", import.meta.url).href
  try {
    for (const generation of [0, 1]) {
      const report = join(directory, `${generation}.json`)
      const program = `import { runClientMemoryProbe } from ${JSON.stringify(source)}; await runClientMemoryProbe(${JSON.stringify(report)}, ${JSON.stringify(directory)}, 2, ${generation === 0})`
      const child = Bun.spawn([process.execPath, "-e", program], { stdout: "pipe", stderr: "pipe", env: { ...process.env, ROTTWEILER_HOME: join(directory, "home") } })
      const [code, stdout, stderr] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()])
      expect({ code, stderr: code === (generation === 0 ? 75 : 0) ? "" : stderr + stdout }).toEqual({ code: generation === 0 ? 75 : 0, stderr: "" })
      const data = JSON.parse(readFileSync(report, "utf8"))
      expect(data.load).toEqual(MEMORY_LOAD)
      expect(data.finalAllocationBytes).toBe(0)
      expect(data.recycle.captured).toBe(generation === 0)
      expect(data.recycle.restored).toBe(generation > 0)
      expect(data.samples.filter((sample: { stage: string }) => sample.stage === "destroyed-and-collected")).toHaveLength(2)
      expect(data.samples.some((sample: { stage: string }) => sample.stage === "decoded-history-and-mutation-awaiting-consumers")).toBe(true)
      expect(data.samples.some((sample: { stage: string }) => sample.stage === "viewer-owned-canonical-page")).toBe(true)
    }
  } finally { rmSync(directory, { recursive: true, force: true }) }
}, 30_000)
