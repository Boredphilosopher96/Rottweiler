import { mkdirSync, mkdtempSync, renameSync, rmSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"

const outputDirectory = join(import.meta.dir, "dist")
const finalExecutable = join(outputDirectory, "rottweiler-plugin-host")
const staging = mkdtempSync(join(tmpdir(), `rottweiler-plugin-host-${process.pid}-`))
const stagedExecutable = join(staging, "rottweiler-plugin-host")
const originalWorkingDirectory = process.cwd()

try {
  process.chdir(staging)
  const result = await Bun.build({
    entrypoints: [join(import.meta.dir, "src/index.ts")],
    compile: {
      outfile: stagedExecutable,
      autoloadDotenv: false,
      autoloadBunfig: false,
      ...(process.platform === "linux"
        ? {
            target:
              process.arch === "arm64"
                ? ("bun-linux-arm64" as const)
                : ("bun-linux-x64-baseline" as const),
          }
        : {}),
    },
    format: "esm",
    minify: true,
    bytecode: true,
  })
  if (!result.success) throw new AggregateError(result.logs, "Bun plugin-host build failed")
  mkdirSync(outputDirectory, { recursive: true })
  rmSync(finalExecutable, { force: true })
  renameSync(stagedExecutable, finalExecutable)
} finally {
  process.chdir(originalWorkingDirectory)
  rmSync(staging, { recursive: true, force: true })
}
