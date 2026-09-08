import { afterEach, expect, test } from "bun:test"
import { readFileSync } from "node:fs"
import { join } from "node:path"
import { JS_HOST_ROLES } from "../../../js-host/generated/release-contract"
import { TestProcessScope } from "../support/owned-process"

const owners: TestProcessScope[] = []
afterEach(async () => { for (const owner of owners.splice(0)) await owner.close() }, 40_000)

test("native rich contributions retrieve remote source content and retire reload/disconnect authority", async () => {
  const owner = await TestProcessScope.create("rw-rich-http-")
  owners.push(owner)
  try {
    const entry = new URL("../../../js-host/src/index.ts", import.meta.url).pathname
    const result = await owner.run([process.execPath, entry, JS_HOST_ROLES.tui], { timeoutMs: 20_000, env: {
      ROTTWEILER_HOME: join(owner.directory, "home"), ROTTWEILER_CLIENT_RICH_PROBE_DIRECTORY: owner.directory,
    } })
    expect({ code: result.code, error: result.code === 0 ? "" : result.stderr + result.stdout }).toEqual({ code: 0, error: "" })
    const report = JSON.parse(readFileSync(join(owner.directory, "rich-http.json"), "utf8"))
    expect(report.finalAllocationBytes).toBe(0)
    expect(report.oracles).toHaveLength(9)
    expect(report.rejectedReads).toContain("get_ui_panels")
  } finally { await owner.close() }
}, 60_000)
