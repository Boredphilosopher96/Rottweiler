import parserWorker from "../node_modules/@opentui/core/parser.worker.js" with { type: "file" }
import javascriptHighlights from "../node_modules/@opentui/core/assets/javascript/highlights.scm" with { type: "file" }
import javascriptWasm from "../node_modules/@opentui/core/assets/javascript/tree-sitter-javascript.wasm" with { type: "file" }
import markdownHighlights from "../node_modules/@opentui/core/assets/markdown/highlights.scm" with { type: "file" }
import markdownInjections from "../node_modules/@opentui/core/assets/markdown/injections.scm" with { type: "file" }
import markdownWasm from "../node_modules/@opentui/core/assets/markdown/tree-sitter-markdown.wasm" with { type: "file" }
import markdownInlineHighlights from "../node_modules/@opentui/core/assets/markdown_inline/highlights.scm" with { type: "file" }
import markdownInlineWasm from "../node_modules/@opentui/core/assets/markdown_inline/tree-sitter-markdown_inline.wasm" with { type: "file" }
import typescriptHighlights from "../node_modules/@opentui/core/assets/typescript/highlights.scm" with { type: "file" }
import typescriptWasm from "../node_modules/@opentui/core/assets/typescript/tree-sitter-typescript.wasm" with { type: "file" }
import zigHighlights from "../node_modules/@opentui/core/assets/zig/highlights.scm" with { type: "file" }
import zigWasm from "../node_modules/@opentui/core/assets/zig/tree-sitter-zig.wasm" with { type: "file" }
import webTreeSitterModule from "../node_modules/web-tree-sitter/tree-sitter.js" with { type: "file" }
import webTreeSitterWasm from "../node_modules/web-tree-sitter/tree-sitter.wasm" with { type: "file" }

import { chmod, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises"
import { lstatSync, readdirSync, rmSync } from "node:fs"
import { tmpdir } from "node:os"
import { dirname, join } from "node:path"

const MAX_ASSET_BYTES = 8 * 1024 * 1024
const MAX_RUNTIME_BYTES = 32 * 1024 * 1024

const embeddedAssets = [
  ["parser.worker.js", parserWorker],
  ["assets/javascript/highlights.scm", javascriptHighlights],
  ["assets/javascript/tree-sitter-javascript.wasm", javascriptWasm],
  ["assets/markdown/highlights.scm", markdownHighlights],
  ["assets/markdown/injections.scm", markdownInjections],
  ["assets/markdown/tree-sitter-markdown.wasm", markdownWasm],
  ["assets/markdown_inline/highlights.scm", markdownInlineHighlights],
  ["assets/markdown_inline/tree-sitter-markdown_inline.wasm", markdownInlineWasm],
  ["assets/typescript/highlights.scm", typescriptHighlights],
  ["assets/typescript/tree-sitter-typescript.wasm", typescriptWasm],
  ["assets/zig/highlights.scm", zigHighlights],
  ["assets/zig/tree-sitter-zig.wasm", zigWasm],
  ["node_modules/web-tree-sitter/tree-sitter.js", webTreeSitterModule],
  ["node_modules/web-tree-sitter/tree-sitter.wasm", webTreeSitterWasm],
] as const

export interface MaterializedTreeSitterRuntime {
  readonly root: string
  readonly workerPath: string
  readonly assetsPath: string
  cleanup(): Promise<void>
  cleanupSync(): void
}

export function embeddedParserConfigurations(assets: string) {
  return [
    {
      filetype: "javascript",
      aliases: ["javascriptreact"],
      queries: { highlights: [join(assets, "javascript/highlights.scm")] },
      wasm: join(assets, "javascript/tree-sitter-javascript.wasm"),
    },
    {
      filetype: "typescript",
      aliases: ["typescriptreact"],
      queries: { highlights: [join(assets, "typescript/highlights.scm")] },
      wasm: join(assets, "typescript/tree-sitter-typescript.wasm"),
    },
    {
      filetype: "markdown_inline",
      queries: { highlights: [join(assets, "markdown_inline/highlights.scm")] },
      wasm: join(assets, "markdown_inline/tree-sitter-markdown_inline.wasm"),
    },
    {
      filetype: "markdown",
      queries: {
        highlights: [join(assets, "markdown/highlights.scm")],
        injections: [join(assets, "markdown/injections.scm")],
      },
      wasm: join(assets, "markdown/tree-sitter-markdown.wasm"),
      injectionMapping: {
        nodeTypes: { inline: "markdown_inline", pipe_table_cell: "markdown_inline" },
        infoStringMap: {
          javascript: "javascript", js: "javascript", jsx: "javascriptreact",
          javascriptreact: "javascriptreact", typescript: "typescript", ts: "typescript",
          tsx: "typescriptreact", typescriptreact: "typescriptreact",
          markdown: "markdown", md: "markdown",
        },
      },
    },
    {
      filetype: "zig",
      queries: { highlights: [join(assets, "zig/highlights.scm")] },
      wasm: join(assets, "zig/tree-sitter-zig.wasm"),
    },
  ]
}

/** Materialize Bun-embedded parser assets for OpenTUI's path-based worker API. */
export async function materializeTreeSitterRuntime(): Promise<MaterializedTreeSitterRuntime> {
  cleanupStaleTreeSitterRuntimes()
  const root = await mkdtemp(join(tmpdir(), `rottweiler-tree-sitter-${process.pid}-`))
  await chmod(root, 0o700)
  let total = 0
  try {
    for (const [relative, embeddedPath] of embeddedAssets) {
      let bytes = new Uint8Array(await Bun.file(embeddedPath).arrayBuffer())
      if (relative === "parser.worker.js") {
        const source = new TextDecoder().decode(bytes)
        const external = 'from "web-tree-sitter"'
        if (source.split(external).length !== 2) {
          throw new Error("embedded Tree-sitter worker has an unexpected dependency shape")
        }
        bytes = new TextEncoder().encode(
          source.replace(external, 'from "./node_modules/web-tree-sitter/tree-sitter.js"'),
        )
      }
      if (bytes.byteLength === 0 || bytes.byteLength > MAX_ASSET_BYTES) {
        throw new Error(`embedded Tree-sitter asset has invalid size: ${relative}`)
      }
      total += bytes.byteLength
      if (total > MAX_RUNTIME_BYTES) {
        throw new Error("embedded Tree-sitter runtime exceeds its size limit")
      }
      const target = join(root, ...relative.split("/"))
      const directory = dirname(target)
      await mkdir(directory, { recursive: true, mode: 0o700 })
      await writeFile(target, bytes, { flag: "wx", mode: 0o600 })
    }
    const packageManifest = new TextEncoder().encode(JSON.stringify({
      name: "web-tree-sitter",
      type: "module",
      exports: {
        ".": "./tree-sitter.js",
        "./tree-sitter.wasm": "./tree-sitter.wasm",
      },
    }))
    total += packageManifest.byteLength
    if (total > MAX_RUNTIME_BYTES) throw new Error("embedded Tree-sitter runtime exceeds its size limit")
    await writeFile(join(root, "node_modules/web-tree-sitter/package.json"), packageManifest, {
      flag: "wx",
      mode: 0o600,
    })
  } catch (error) {
    await rm(root, { recursive: true, force: true })
    throw error
  }
  let cleaned = false
  const cleanupSync = () => {
    if (cleaned) return
    cleaned = true
    rmSync(root, { recursive: true, force: true })
  }
  process.once("exit", cleanupSync)
  return {
    root,
    workerPath: join(root, "parser.worker.js"),
    assetsPath: join(root, "assets"),
    async cleanup() {
      if (cleaned) return
      await rm(root, { recursive: true, force: true })
      cleaned = true
      process.off("exit", cleanupSync)
    },
    cleanupSync,
  }
}

function cleanupStaleTreeSitterRuntimes(): void {
  const directory = tmpdir()
  const currentUid = typeof process.getuid === "function" ? process.getuid() : null
  for (const entry of readdirSync(directory)) {
    const match = /^rottweiler-tree-sitter-(\d+)-/.exec(entry)
    if (match?.[1] === undefined) continue
    const ownerPid = Number(match[1])
    let running = false
    try {
      process.kill(ownerPid, 0)
      running = true
    } catch (error) {
      running = (error as NodeJS.ErrnoException).code === "EPERM"
    }
    if (running) continue
    const path = join(directory, entry)
    let metadata
    try {
      metadata = lstatSync(path)
    } catch {
      continue
    }
    // Never follow or remove attacker-controlled links/special files. Only a
    // private directory owned by this uid with the exact creation mode qualifies.
    if (
      !metadata.isDirectory() ||
      metadata.isSymbolicLink() ||
      (metadata.mode & 0o777) !== 0o700 ||
      (currentUid !== null && metadata.uid !== currentUid)
    ) continue
    rmSync(path, { recursive: true, force: true })
  }
}
