import { expect, test } from "bun:test"
import { mkdtemp, readFile, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { runOwnedProcess } from "./support/owned-process"

test("test process owner drains both pipes and reports a nonzero exit", async () => {
  const result = await runOwnedProcess([process.execPath, "-e", 'console.log("out");console.error("err");process.exit(7)'], { timeoutMs: 2_000 })
  expect(result).toEqual({ code: 7, stdout: "out\n", stderr: "err\n" })
})

for (const mode of ["timeout", "output"] as const) {
  test(`test process owner reaps a child before reporting ${mode} failure`, async () => {
    const directory = await mkdtemp(join(tmpdir(), "rw-owned-process-"))
    const pidFile = join(directory, "pid")
    try {
      const behavior = mode === "timeout" ? 'setInterval(() => {}, 100)' : 'setInterval(() => process.stdout.write("x".repeat(1024)), 1)'
      const program = `import {writeFileSync} from "node:fs";writeFileSync(${JSON.stringify(pidFile)}, String(process.pid));${behavior}`
      await expect(runOwnedProcess([process.execPath, "-e", program], {
        timeoutMs: 2_000, maxOutputBytes: 2_048,
      })).rejects.toThrow(mode === "timeout" ? "exceeded 2000ms" : "output exceeded")
      const pid = Number(await readFile(pidFile, "utf8"))
      expect(() => process.kill(pid, 0)).toThrow()
    } finally {
      await rm(directory, { recursive: true, force: true })
    }
  })
}
