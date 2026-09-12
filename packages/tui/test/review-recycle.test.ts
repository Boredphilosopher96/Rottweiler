import { afterEach, expect, test } from "bun:test"
import { readFileSync } from "node:fs"
import { join } from "node:path"
import { JS_HOST_ROLES } from "../../js-host/generated/release-contract"
import { TestProcessScope } from "./support/owned-process"

const owners: TestProcessScope[] = []
afterEach(async () => { for (const owner of owners.splice(0)) await owner.close() }, 40_000)

for (const view of ["session", "workspace"] as const) for (const mode of ["restore", "changed", "removed"] as const) {
  test(`actual App/HTTP review survives process replacement: ${view}/${mode}`, async () => {
    const owner = await TestProcessScope.create("rw-review-recycle-")
    owners.push(owner)
    try {
      for (const phase of ["capture", mode] as const) {
        const entry = new URL("../../js-host/src/index.ts", import.meta.url).pathname
        const result = await owner.run([process.execPath, entry, JS_HOST_ROLES.tui], { timeoutMs: 20_000,
          env: { ROTTWEILER_HOME: join(owner.directory, "home"), ROTTWEILER_CLIENT_REVIEW_PROBE_DIRECTORY: owner.directory,
            ROTTWEILER_CLIENT_REVIEW_PROBE_MODE: phase, ROTTWEILER_CLIENT_REVIEW_PROBE_VIEW: view } })
        expect({ code: result.code, error: result.code === (phase === "capture" ? 75 : 0) ? "" : result.stderr + result.stdout })
          .toEqual({ code: phase === "capture" ? 75 : 0, error: "" })
        const report = JSON.parse(readFileSync(join(owner.directory, `${phase}.json`), "utf8"))
        expect(report.finalAllocationBytes).toBe(0)
        if (phase === "capture" || phase === "restore") expect(report.observed).toMatchObject({ path: "held-review2.txt", scrollTop: 40 })
        else expect(report.observed).toMatchObject({ refused: true })
        if (phase === "restore") expect(report.decisions).toBe(view === "session" ? 1 : 0)
      }
    } finally { await owner.close() }
  }, 60_000)
}
