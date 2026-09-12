import { plugin } from "bun"
import { isAbsolute, resolve } from "node:path"

// Register without importing OpenTUI: a source-plugin role in this directory
// must remain independent of terminal initialization and native preparation.
let verified: string | undefined
function nativeLibrary(): string {
  if (verified !== undefined) return verified
  const verification = Bun.spawnSync(["python3", resolve(import.meta.dir, "../../../scripts/verify-opentui-native.py")], {
    stdout: "pipe", stderr: "pipe",
  })
  if (verification.exitCode !== 0) throw new Error(verification.stderr.toString().trim())
  const library = verification.stdout.toString().trim()
  if (!isAbsolute(library)) throw new Error("native verification did not return an absolute library")
  verified = library
  return library
}

plugin({
  name: "rottweiler-source-native",
  setup(build) {
    // Runtime package imports need explicit module registration; bundler-only
    // resolution hooks do not intercept Bun's native-loaded dependency graph.
    for (const specifier of ["rottweiler-opentui-native", `@opentui/core-${process.platform}-${process.arch}`]) {
      build.module(specifier, () => ({ loader: "object", exports: { default: nativeLibrary() } }))
    }
  },
})
