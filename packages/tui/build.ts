import { copyFileSync, mkdirSync, readdirSync, rmSync } from "node:fs"
import { dirname, join } from "node:path"

import type { BunPlugin } from "bun"

function cleanupOrphanedBunBuilds(): void {
  for (const entry of readdirSync(import.meta.dir)) {
    if (/^\..+\.bun-build$/.test(entry)) {
      // Bun normally leaves a file here, but interrupted/compiler-version
      // changes have also produced directories. Handle both shapes.
      rmSync(join(import.meta.dir, entry), { recursive: true, force: true })
    }
  }
}

// Bun's single-executable compiler can leave large hidden temporary binaries
// after an interrupted build. Clean on both sides so the next successful build
// repairs a prior interruption and does not grow the repository indefinitely.
cleanupOrphanedBunBuilds()

function nativePackage(): string {
  if (process.platform === "darwin" && process.arch === "x64") return "@opentui/core-darwin-x64"
  if (process.platform === "darwin" && process.arch === "arm64") return "@opentui/core-darwin-arm64"
  if (process.platform === "win32" && process.arch === "x64") return "@opentui/core-win32-x64"
  if (process.platform === "win32" && process.arch === "arm64") return "@opentui/core-win32-arm64"
  if (process.platform === "linux") {
    const libc = process.env.OPENTUI_LIBC
    if (libc !== undefined && libc !== "" && libc !== "glibc" && libc !== "musl") {
      throw new Error(`OPENTUI_LIBC must be glibc or musl, got ${libc}`)
    }
    const suffix = libc === "musl" ? "-musl" : ""
    if (process.arch === "x64") return `@opentui/core-linux-x64${suffix}`
    if (process.arch === "arm64") return `@opentui/core-linux-arm64${suffix}`
  }
  throw new Error(`OpenTUI does not support ${process.platform}-${process.arch}`)
}

const selectedNativePackage = nativePackage()
const selectedNativeEntry = Bun.resolveSync(selectedNativePackage, import.meta.dir)
const selectedNativeDirectory = dirname(selectedNativeEntry)
const selectedNativeLibrary =
  process.platform === "win32"
    ? "opentui.dll"
    : process.platform === "darwin"
      ? "libopentui.dylib"
      : "libopentui.so"
const selectedNativePath = join(selectedNativeDirectory, selectedNativeLibrary)
const nativePrelude: BunPlugin = {
  name: "rottweiler-opentui-native",
  setup(build) {
    build.onResolve({ filter: /^rottweiler-opentui-native$/ }, () => ({
      path: "rottweiler-opentui-native",
      namespace: "rottweiler-native",
    }))
    build.onLoad({ filter: /.*/, namespace: "rottweiler-native" }, () => ({
      loader: "js",
      contents: `import { dirname, join } from "node:path"; globalThis.__rottweilerOpenTuiNativeLibrary = join(dirname(process.execPath), ${JSON.stringify(selectedNativeLibrary)});`,
    }))
  },
}

const outputDirectory = "dist"
const outputExecutable = join(outputDirectory, "rottweiler-tui")
const result = await Bun.build({
  entrypoints: ["src/index.ts"],
  compile: {
    outfile: outputExecutable,
    autoloadDotenv: false,
    autoloadBunfig: false,
  },
  format: "esm",
  minify: true,
  bytecode: true,
  plugins: [nativePrelude],
}).finally(cleanupOrphanedBunBuilds)

if (!result.success) {
  for (const message of result.logs) console.error(message)
  process.exit(1)
}

mkdirSync(outputDirectory, { recursive: true })
copyFileSync(selectedNativePath, join(outputDirectory, selectedNativeLibrary))
