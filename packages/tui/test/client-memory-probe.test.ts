import { afterEach, expect, test } from "bun:test"
import { readFileSync } from "node:fs"
import { join } from "node:path"
import { MEMORY_LOAD } from "../src/diagnostics/memory-fixture"

import { TestProcessScope } from "./support/owned-process"

const owners: TestProcessScope[] = []
// A 20s child deadline plus bounded group teardown and supervisor startup.
afterEach(async () => { for (const owner of owners.splice(0)) await owner.close() }, 40_000)

test("actual App/HTTP ownership workload retires each generation and restores process handoff", async () => {
  const owner = await TestProcessScope.create("rw-client-memory-test-")
  owners.push(owner)
  const directory = owner.directory
  const source = new URL("../src/diagnostics/memory-probe.ts", import.meta.url).href
  try {
    for (const generation of [0, 1]) {
      const report = join(directory, `${generation}.json`)
      const program = `import { runClientMemoryProbe } from ${JSON.stringify(source)}; await runClientMemoryProbe(${JSON.stringify(report)}, ${JSON.stringify(directory)}, 2, ${generation === 0})`
      const { code, stdout, stderr } = await owner.run([process.execPath, "-e", program], {
        timeoutMs: 20_000, env: { ROTTWEILER_HOME: join(directory, "home") },
      })
      expect({ code, stderr: code === (generation === 0 ? 75 : 0) ? "" : stderr + stdout }).toEqual({ code: generation === 0 ? 75 : 0, stderr: "" })
      const data = JSON.parse(readFileSync(report, "utf8"))
      expect(data.load).toEqual(MEMORY_LOAD)
      expect(data.collection).toBe("production-policy")
      expect(data.handoffAttachmentBytes).toBeGreaterThan(4 * 1024 * 1024)
      expect(data.history.observations.at(-1)).toMatchObject({ stage: "search-match", anchor: "5001" })
      expect(data.finalAllocationBytes).toBe(0)
      expect(data.resolvedChildControls).toBe(0)
      expect(data.recycle.captured).toBe(generation === 0)
      expect(data.recycle.restored).toBe(generation > 0)
      expect(data.samples.filter((sample: { stage: string }) => sample.stage === "destroyed")).toHaveLength(2)
      expect(data.samples.some((sample: { stage: string }) => sample.stage === "decoded-history-and-mutation-awaiting-consumers")).toBe(true)
      expect(data.samples.some((sample: { stage: string }) => sample.stage === "viewer-owned-canonical-page")).toBe(true)
      expect(data.samples.some((sample: { stage: string }) => sample.stage === "pending-child-before-process-handoff")).toBe(true)
      expect(data.samples.some((sample: { stage: string }) => sample.stage === "restored-pending-child-with-authoritative-controls")).toBe(generation > 0)
    }
  } finally { await owner.close() }
}, 60_000)


test("held output, review, secret entry and unsettled actions survive streaming without losing their owners", async () => {
  const owner = await TestProcessScope.create("rw-held-memory-test-")
  owners.push(owner)
  const directory = owner.directory
  const source = new URL("../src/diagnostics/memory-held.ts", import.meta.url).href
  try {
    for (const view of ["output", "review", "secret", "action"]) {
      const report = join(directory, `${view}.json`)
      const program = `import { runHeldViewMemoryProbe } from ${JSON.stringify(source)}; await runHeldViewMemoryProbe(${JSON.stringify(report)}, ${JSON.stringify(directory)}, 2, ${JSON.stringify(view)})`
      const { code, stdout, stderr } = await owner.run([process.execPath, "-e", program], {
        timeoutMs: 20_000, env: { ROTTWEILER_HOME: join(directory, "home") },
      })
      expect({ code, stderr: code === 0 ? "" : stderr + stdout }).toEqual({ code: 0, stderr: "" })
      const data = JSON.parse(readFileSync(report, "utf8"))
      expect(data.view).toBe(view)
      expect(data.finalAllocationBytes).toBe(0)
      expect(data.samples).toHaveLength(2)
      expect(data.samples.every((sample: { terminal: { queuedBytes: number; bytes: number } }) =>
        sample.terminal.queuedBytes === 0 && sample.terminal.bytes > 0)).toBe(true)
    }
  } finally { await owner.close() }
}, 120_000)
