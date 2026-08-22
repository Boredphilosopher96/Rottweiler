import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
} from "node:fs"
import { spawnSync } from "node:child_process"
import { createHash } from "node:crypto"
import { tmpdir } from "node:os"
import { dirname, isAbsolute, join } from "node:path"

import type { BunPlugin } from "bun"

const MAX_EMBEDDED_TREE_SITTER_ASSET_BYTES = 8 * 1024 * 1024
const COMPRESSED_TREE_SITTER_ASSET_HEADER_BYTES = 8

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
const treeSitterAssetDigest = createHash("sha256")
  .update(readFileSync(join(import.meta.dir, "bun.lock")))
  .update(readFileSync(join(import.meta.dir, "src/tree-sitter-runtime.ts")))
  .update(readFileSync(join(import.meta.dir, "src/tree-sitter-client.ts")))
  .digest("hex")
function stripLinuxNativeLibrary(path: string): void {
  if (process.platform !== "linux") return
  const stripExecutable = process.env.ROTTWEILER_STRIP_BIN ?? "/usr/bin/strip"
  if (!isAbsolute(stripExecutable)) {
    throw new Error("ROTTWEILER_STRIP_BIN must be an absolute path")
  }
  const stripped = spawnSync(stripExecutable, ["--strip-unneeded", path], {
    encoding: "utf8",
  })
  if (stripped.error !== undefined) {
    throw new Error(`failed to strip Linux OpenTUI native library: ${stripped.error.message}`)
  }
  if (stripped.status !== 0) {
    const detail = stripped.stderr.trim()
    throw new Error(
      `failed to strip Linux OpenTUI native library (exit ${stripped.status})${detail === "" ? "" : `: ${detail}`}`,
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

function enforceTuiBundleSize(executable: string, nativeLibrary: string): void {
  const limit =
    process.platform === "darwin"
      ? 100_000_000
      : process.platform === "linux"
        ? 150_000_000
        : undefined
  if (limit === undefined) return
  const bundleBytes = statSync(executable).size + statSync(nativeLibrary).size
  if (bundleBytes >= limit) {
    throw new Error(`release TUI bundle is ${bundleBytes} bytes; budget is <${limit}`)
  }
  console.log(`Release TUI bundle bytes: ${bundleBytes} (budget <${limit})`)
}

const compressedTreeSitterAssets: BunPlugin = {
  name: "rottweiler-compressed-tree-sitter-assets",
  setup(build) {
    const compress = async ({ path }: { path: string }) => {
      const source = new Uint8Array(await Bun.file(path).arrayBuffer())
      if (
        source.byteLength === 0 ||
        source.byteLength > MAX_EMBEDDED_TREE_SITTER_ASSET_BYTES
      ) {
        throw new Error(`Tree-sitter asset has invalid size before compression: ${path}`)
      }
      const compressed = Bun.zstdCompressSync(source, { level: 19 })
      const contents = new Uint8Array(
        COMPRESSED_TREE_SITTER_ASSET_HEADER_BYTES + compressed.byteLength,
      )
      contents.set([0x52, 0x57, 0x54, 0x5a])
      new DataView(contents.buffer).setUint32(4, source.byteLength, true)
      contents.set(compressed, COMPRESSED_TREE_SITTER_ASSET_HEADER_BYTES)
      return { contents, loader: "file" as const }
    }
    build.onLoad({ filter: /\.(?:wasm|scm)$/ }, compress)
    build.onLoad({ filter: /(?:parser\.worker|tree-sitter)\.js$/ }, compress)
  },
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
// Remove obsolete sidecar parser bundles from older builds. The signed release
// contract permits only the executable and native renderer in this directory.
rmSync(join(outputDirectory, "parser.worker.js"), { force: true })
rmSync(join(outputDirectory, "assets"), { recursive: true, force: true })
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
      ...(process.platform === "linux"
        ? { target: "bun-linux-x64-baseline" as const }
        : {}),
    },
    format: "esm",
    minify: true,
    bytecode: true,
    define: {
      __ROTTWEILER_TREE_SITTER_ASSET_DIGEST__: JSON.stringify(`sha256-${treeSitterAssetDigest}`),
    },
    plugins: [compressedTreeSitterAssets, nativePrelude],
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

if (process.platform === "linux") {
  console.log(`Linux Bun compiled output bytes: ${statSync(outputExecutable).size}`)
}

mkdirSync(outputDirectory, { recursive: true })
const outputNativePath = join(outputDirectory, selectedNativeLibrary)
copyFileSync(selectedNativePath, outputNativePath)
stripLinuxNativeLibrary(outputNativePath)
signDarwinArtifact(outputExecutable, "OpenTUI executable")
signDarwinArtifact(outputNativePath, "OpenTUI native library")
enforceTuiBundleSize(outputExecutable, outputNativePath)

// Prove the compiled release executable contains its parser runtime. Only the
// native renderer remains adjacent, preserving the signed six-entry archive.
const smokeDirectory = mkdtempSync(join(tmpdir(), "rottweiler-embedded-parser-smoke-"))
try {
  const smokeExecutable = join(smokeDirectory, "rottweiler-tui")
  const smokeNative = join(smokeDirectory, selectedNativeLibrary)
  const smokeReport = join(smokeDirectory, "report.json")
  const smokeHome = join(smokeDirectory, "home")
  copyFileSync(outputExecutable, smokeExecutable)
  copyFileSync(outputNativePath, smokeNative)
  const smoke = spawnSync(smokeExecutable, [], {
    cwd: smokeDirectory,
    encoding: "utf8",
    timeout: 30_000,
    env: {
      ...process.env,
      ROTTWEILER_HOME: smokeHome,
      ROTTWEILER_TREE_SITTER_SMOKE_REPORT: smokeReport,
    },
  })
  if (smoke.error !== undefined || smoke.status !== 0 || !existsSync(smokeReport)) {
    throw new Error(
      `compiled embedded-parser smoke failed${smoke.error === undefined ? ` (exit ${smoke.status})` : `: ${smoke.error.message}`}\n${smoke.stderr}`,
    )
  }
  const report = JSON.parse(readFileSync(smokeReport, "utf8")) as { frame?: string }
  if (!report.frame?.includes("const answer = 42")) {
    throw new Error("compiled embedded-parser smoke did not report highlighted fenced code")
  }
  rmSync(smokeHome, { recursive: true, force: true })
  const entries = readdirSync(smokeDirectory).sort()
  if (entries.join("\n") !== [selectedNativeLibrary, "report.json", "rottweiler-tui"].sort().join("\n")) {
    throw new Error("compiled TUI required unexpected adjacent parser assets")
  }
} finally {
  rmSync(smokeDirectory, { recursive: true, force: true })
}
