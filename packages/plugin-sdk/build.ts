import { cpSync, mkdirSync, mkdtempSync, readdirSync, rmSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"

const prefix = "rottweiler-plugin-sdk-build-"
for (const entry of readdirSync(tmpdir(), { withFileTypes: true })) {
  const match = new RegExp(`^${prefix}(\\d+)-`).exec(entry.name)
  if (!entry.isDirectory() || match?.[1] === undefined) continue
  let running = false
  try {
    process.kill(Number(match[1]), 0)
    running = true
  } catch (error) {
    running = (error as NodeJS.ErrnoException).code === "EPERM"
  }
  if (!running) rmSync(join(tmpdir(), entry.name), { recursive: true, force: true })
}

const stage = mkdtempSync(join(tmpdir(), `${prefix}${process.pid}-`))
const js = join(stage, "js")
const types = join(stage, "types")
const cleanup = () => rmSync(stage, { recursive: true, force: true })
const onSignal = (signal: NodeJS.Signals) => {
  cleanup()
  process.exit(signal === "SIGINT" ? 130 : 143)
}
process.once("SIGINT", onSignal)
process.once("SIGTERM", onSignal)

try {
  if (process.env.ROTTWEILER_SDK_TEST_FAIL_AFTER_STAGE === "1") {
    throw new Error("injected SDK staging failure")
  }
  const built = await Bun.build({
    entrypoints: ["src/index.ts", "src/scaffold.ts", "src/bin/scaffold.ts"],
    outdir: js,
    root: "src",
    target: "bun",
    format: "esm",
    splitting: true,
    minify: true,
  })
  if (!built.success) throw new AggregateError(built.logs, "Bun SDK build failed")

  const tsc = Bun.spawnSync([
    join(import.meta.dir, "node_modules", ".bin", "tsc"),
    "--project",
    "tsconfig.build.json",
    "--outDir",
    types,
  ])
  if (tsc.exitCode !== 0) {
    const diagnostics = `${tsc.stdout.toString()}\n${tsc.stderr.toString()}`.trim().slice(0, 16 * 1024)
    throw new Error(`TypeScript declaration build failed${diagnostics === "" ? "" : `:\n${diagnostics}`}`)
  }

  rmSync("dist", { recursive: true, force: true })
  mkdirSync("dist", { recursive: true })
  cpSync(js, "dist", { recursive: true })
  cpSync(types, "dist", { recursive: true })
} finally {
  process.off("SIGINT", onSignal)
  process.off("SIGTERM", onSignal)
  cleanup()
}
