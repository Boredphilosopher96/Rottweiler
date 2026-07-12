import { copyFileSync, mkdirSync, mkdtempSync, readdirSync, rmSync } from "node:fs"
import { spawnSync } from "node:child_process"
import { tmpdir } from "node:os"
import { dirname, isAbsolute, join } from "node:path"

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

function cleanupOrphanedTempBuilds(): void {
  for (const entry of readdirSync(tmpdir(), { withFileTypes: true })) {
    const match = /^rottweiler-bun-build-(\d+)-/.exec(entry.name)
    if (!entry.isDirectory() || match?.[1] === undefined) continue
    const owner = Number(match[1])
    let ownerIsRunning = false
    try {
      process.kill(owner, 0)
      ownerIsRunning = true
    } catch (error) {
      ownerIsRunning = (error as NodeJS.ErrnoException).code === "EPERM"
    }
    if (!ownerIsRunning) {
      rmSync(join(tmpdir(), entry.name), { recursive: true, force: true })
    }
  }
}

// Bun's single-executable compiler can leave large hidden temporary binaries
// after an interrupted build. Clean on both sides so the next successful build
// repairs a prior interruption and does not grow the repository indefinitely.
cleanupOrphanedBunBuilds()
cleanupOrphanedTempBuilds()

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
function stripLinuxArtifact(path: string, label: string): void {
  if (process.platform !== "linux") return
  const stripExecutable = process.env.ROTTWEILER_STRIP_BIN ?? "/usr/bin/strip"
  if (!isAbsolute(stripExecutable)) {
    throw new Error("ROTTWEILER_STRIP_BIN must be an absolute path")
  }
  const stripped = spawnSync(stripExecutable, ["--strip-unneeded", path], {
    encoding: "utf8",
  })
  if (stripped.error !== undefined) {
    throw new Error(`failed to strip Linux ${label}: ${stripped.error.message}`)
  }
  if (stripped.status !== 0) {
    const detail = stripped.stderr.trim()
    throw new Error(
      `failed to strip Linux ${label} (exit ${stripped.status})${detail === "" ? "" : `: ${detail}`}`,
    )
  }
}

function signDarwinArtifact(path: string, label: string): void {
  if (process.platform !== "darwin") return
  const signed = spawnSync(
    "/usr/bin/codesign",
    ["--force", "--sign", "-", "--timestamp=none", path],
    { encoding: "utf8" },
  )
  if (signed.error !== undefined) {
    throw new Error(`failed to sign macOS ${label}: ${signed.error.message}`)
  }
  if (signed.status !== 0) {
    const detail = signed.stderr.trim()
    throw new Error(
      `failed to sign macOS ${label} (exit ${signed.status})${detail === "" ? "" : `: ${detail}`}`,
    )
  }
}
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
const compilationDirectory = mkdtempSync(join(tmpdir(), `rottweiler-bun-build-${process.pid}-`))
const originalWorkingDirectory = process.cwd()
let result: Awaited<ReturnType<typeof Bun.build>>
const cleanupCompilationDirectory = () => {
  rmSync(compilationDirectory, { recursive: true, force: true })
}
const interruptBuild = (signal: NodeJS.Signals) => {
  process.chdir(originalWorkingDirectory)
  cleanupCompilationDirectory()
  process.exit(signal === "SIGINT" ? 130 : 143)
}

process.once("SIGINT", interruptBuild)
process.once("SIGTERM", interruptBuild)

try {
  // Bun's executable compiler creates a large hidden `.*.bun-build` staging
  // artifact in its working directory. Keep it out of the checkout so repeated
  // builds do not inflate the workspace, even while a build is in progress.
  process.chdir(compilationDirectory)
  result = await Bun.build({
    entrypoints: [join(import.meta.dir, "src/index.ts")],
    compile: {
      outfile: join(import.meta.dir, outputExecutable),
      autoloadDotenv: false,
      autoloadBunfig: false,
    },
    format: "esm",
    minify: true,
    bytecode: true,
    plugins: [nativePrelude],
  })
} finally {
  process.off("SIGINT", interruptBuild)
  process.off("SIGTERM", interruptBuild)
  process.chdir(originalWorkingDirectory)
  cleanupCompilationDirectory()
  cleanupOrphanedBunBuilds()
}

if (!result.success) {
  for (const message of result.logs) console.error(message)
  process.exit(1)
}

mkdirSync(outputDirectory, { recursive: true })
const outputNativePath = join(outputDirectory, selectedNativeLibrary)
copyFileSync(selectedNativePath, outputNativePath)
stripLinuxArtifact(outputNativePath, "OpenTUI native library")
signDarwinArtifact(outputExecutable, "OpenTUI executable")
signDarwinArtifact(outputNativePath, "OpenTUI native library")
