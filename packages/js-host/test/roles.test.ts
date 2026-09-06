import { expect, test } from "bun:test"
import { mkdtemp, readdir, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join, resolve } from "node:path"
import { JS_HOST_ROLES } from "../generated/release-contract"

import { SOURCE_HOST_ABI, SOURCE_BUNDLE_FORMAT } from "../../plugin-host/src/protocol"

const entry = resolve(import.meta.dir, "../src/index.ts")

async function invoke(args: readonly string[]) {
  const root = await mkdtemp(join(tmpdir(), "rw-js-role-"))
  try {
    const preload = join(root, "reject-terminal.ts")
    await writeFile(preload, `import { plugin } from "bun";
plugin({name:"reject-terminal",setup(build){build.onResolve({filter:/opentui|tree-sitter/},()=>{throw new Error("terminal dependency loaded")})}});
`)
    const child = Bun.spawn([process.execPath, "--preload", preload, entry, ...args], {
      cwd: root, stdout: "pipe", stderr: "pipe",
      env: { ...process.env, ROTTWEILER_HOME: join(root, "home"), ROTTWEILER_TREE_SITTER_SMOKE_REPORT: join(root, "parser-report.json") },
    })
    const timer = setTimeout(() => child.kill("SIGKILL"), 3_000)
    const [status, stdout, stderr] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()])
      .finally(() => clearTimeout(timer))
    expect(await readdir(root)).toEqual(["reject-terminal.ts"])
    return { status, stdout, stderr }
  } finally { await rm(root, { recursive: true, force: true }) }
}

test("source-plugin loads no terminal dependency and writes only its protocol response", async () => {
  expect(await invoke([JS_HOST_ROLES.source_plugin, "version"])).toEqual({
    status: 0, stdout: `${JSON.stringify({ abi: SOURCE_HOST_ABI, format: SOURCE_BUNDLE_FORMAT })}\n`, stderr: "",
  })
})

test("missing or unsupported roles reject before application initialization", async () => {
  for (const args of [[], ["version"], ["plugin-host"], [JS_HOST_ROLES.tui, "unexpected"]]) {
    const result = await invoke(args)
    expect(result.status).toBe(1)
    expect(result.stdout).toBe("")
    expect(result.stderr).not.toContain("terminal dependency")
    expect(result.stderr).not.toContain("\u001b")
  }
})
