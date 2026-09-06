import parserWorker from "../node_modules/@opentui/core/parser.worker.js" with { type: "file" }
import bashHighlights from "../node_modules/tree-sitter-bash/queries/highlights.scm" with { type: "file" }
import bashWasm from "../node_modules/tree-sitter-bash/tree-sitter-bash.wasm" with { type: "file" }
import cHighlights from "../node_modules/tree-sitter-c/queries/highlights.scm" with { type: "file" }
import cWasm from "../node_modules/tree-sitter-c/tree-sitter-c.wasm" with { type: "file" }
import cppHighlights from "../node_modules/tree-sitter-cpp/queries/highlights.scm" with { type: "file" }
import cppWasm from "../node_modules/tree-sitter-cpp/tree-sitter-cpp.wasm" with { type: "file" }
import csharpHighlights from "../node_modules/tree-sitter-c-sharp/queries/highlights.scm" with { type: "file" }
import csharpWasm from "../node_modules/tree-sitter-c-sharp/tree-sitter-c_sharp.wasm" with { type: "file" }
import cssHighlights from "../node_modules/tree-sitter-css/queries/highlights.scm" with { type: "file" }
import cssWasm from "../node_modules/tree-sitter-css/tree-sitter-css.wasm" with { type: "file" }
import goHighlights from "../node_modules/tree-sitter-go/queries/highlights.scm" with { type: "file" }
import goWasm from "../node_modules/tree-sitter-go/tree-sitter-go.wasm" with { type: "file" }
import htmlHighlights from "../node_modules/tree-sitter-html/queries/highlights.scm" with { type: "file" }
import htmlWasm from "../node_modules/tree-sitter-html/tree-sitter-html.wasm" with { type: "file" }
import javaHighlights from "../node_modules/tree-sitter-java/queries/highlights.scm" with { type: "file" }
import javaWasm from "../node_modules/tree-sitter-java/tree-sitter-java.wasm" with { type: "file" }
import luaHighlights from "../node_modules/@tree-sitter-grammars/tree-sitter-lua/queries/highlights.scm" with { type: "file" }
import luaWasm from "../node_modules/@tree-sitter-grammars/tree-sitter-lua/tree-sitter-lua.wasm" with { type: "file" }
import makeHighlights from "../node_modules/tree-sitter-make/queries/highlights.scm" with { type: "file" }
import makeWasm from "../node_modules/tree-sitter-make/tree-sitter-make.wasm" with { type: "file" }
import javascriptHighlights from "../node_modules/@opentui/core/assets/javascript/highlights.scm" with { type: "file" }
import javascriptWasm from "../node_modules/@opentui/core/assets/javascript/tree-sitter-javascript.wasm" with { type: "file" }
import jsonHighlights from "../node_modules/tree-sitter-json/queries/highlights.scm" with { type: "file" }
import jsonWasm from "../node_modules/tree-sitter-json/tree-sitter-json.wasm" with { type: "file" }
import markdownHighlights from "../node_modules/@opentui/core/assets/markdown/highlights.scm" with { type: "file" }
import markdownInjections from "../node_modules/@opentui/core/assets/markdown/injections.scm" with { type: "file" }
import markdownWasm from "../node_modules/@opentui/core/assets/markdown/tree-sitter-markdown.wasm" with { type: "file" }
import markdownInlineHighlights from "../node_modules/@opentui/core/assets/markdown_inline/highlights.scm" with { type: "file" }
import markdownInlineWasm from "../node_modules/@opentui/core/assets/markdown_inline/tree-sitter-markdown_inline.wasm" with { type: "file" }
import phpHighlights from "../node_modules/tree-sitter-php/queries/highlights.scm" with { type: "file" }
import phpWasm from "../node_modules/tree-sitter-php/tree-sitter-php.wasm" with { type: "file" }
import pythonHighlights from "../node_modules/tree-sitter-python/queries/highlights.scm" with { type: "file" }
import pythonWasm from "../node_modules/tree-sitter-python/tree-sitter-python.wasm" with { type: "file" }
import rubyHighlights from "../node_modules/tree-sitter-ruby/queries/highlights.scm" with { type: "file" }
import rubyWasm from "../node_modules/tree-sitter-ruby/tree-sitter-ruby.wasm" with { type: "file" }
import rustHighlights from "../node_modules/tree-sitter-rust/queries/highlights.scm" with { type: "file" }
import rustWasm from "../node_modules/tree-sitter-rust/tree-sitter-rust.wasm" with { type: "file" }
import tomlHighlights from "../node_modules/@tree-sitter-grammars/tree-sitter-toml/queries/highlights.scm" with { type: "file" }
import tomlWasm from "../node_modules/@tree-sitter-grammars/tree-sitter-toml/tree-sitter-toml.wasm" with { type: "file" }
import typescriptHighlights from "../node_modules/@opentui/core/assets/typescript/highlights.scm" with { type: "file" }
import typescriptWasm from "../node_modules/@opentui/core/assets/typescript/tree-sitter-typescript.wasm" with { type: "file" }
import yamlHighlights from "../node_modules/@tree-sitter-grammars/tree-sitter-yaml/queries/highlights.scm" with { type: "file" }
import yamlWasm from "../node_modules/@tree-sitter-grammars/tree-sitter-yaml/tree-sitter-yaml.wasm" with { type: "file" }
import zigHighlights from "../node_modules/@opentui/core/assets/zig/highlights.scm" with { type: "file" }
import zigWasm from "../node_modules/@opentui/core/assets/zig/tree-sitter-zig.wasm" with { type: "file" }
import webTreeSitterModule from "../node_modules/web-tree-sitter/tree-sitter.js" with { type: "file" }
import webTreeSitterWasm from "../node_modules/web-tree-sitter/tree-sitter.wasm" with { type: "file" }

import { lstat, mkdir, readFile, rename, rm, writeFile } from "node:fs/promises"
import { randomUUID } from "node:crypto"
import { homedir } from "node:os"
import { dirname, join } from "node:path"

declare const __ROTTWEILER_TREE_SITTER_ASSET_DIGEST__: string

const MAX_ASSET_BYTES = 8 * 1024 * 1024
const MAX_RUNTIME_BYTES = 32 * 1024 * 1024
const COMPRESSED_ASSET_HEADER_BYTES = 8
const TREE_SITTER_ASSET_DIGEST =
  typeof __ROTTWEILER_TREE_SITTER_ASSET_DIGEST__ === "string"
    ? __ROTTWEILER_TREE_SITTER_ASSET_DIGEST__
    : `development-${process.pid}`

const embeddedAssets = [
  ["parser.worker.js", parserWorker],
  // OpenTUI resolves its built-in parsers through the public OTUI_ASSET_ROOT
  // layout before our larger parser catalog replaces those defaults.
  ["@opentui/core/assets/javascript/highlights.scm", javascriptHighlights],
  ["@opentui/core/assets/javascript/tree-sitter-javascript.wasm", javascriptWasm],
  ["@opentui/core/assets/markdown/highlights.scm", markdownHighlights],
  ["@opentui/core/assets/markdown/injections.scm", markdownInjections],
  ["@opentui/core/assets/markdown/tree-sitter-markdown.wasm", markdownWasm],
  ["@opentui/core/assets/markdown_inline/highlights.scm", markdownInlineHighlights],
  ["@opentui/core/assets/markdown_inline/tree-sitter-markdown_inline.wasm", markdownInlineWasm],
  ["@opentui/core/assets/typescript/highlights.scm", typescriptHighlights],
  ["@opentui/core/assets/typescript/tree-sitter-typescript.wasm", typescriptWasm],
  ["@opentui/core/assets/zig/highlights.scm", zigHighlights],
  ["@opentui/core/assets/zig/tree-sitter-zig.wasm", zigWasm],
  ["assets/bash/highlights.scm", bashHighlights],
  ["assets/bash/tree-sitter-bash.wasm", bashWasm],
  ["assets/c/highlights.scm", cHighlights],
  ["assets/c/tree-sitter-c.wasm", cWasm],
  ["assets/cpp/highlights.scm", cppHighlights],
  ["assets/cpp/tree-sitter-cpp.wasm", cppWasm],
  ["assets/csharp/highlights.scm", csharpHighlights],
  ["assets/csharp/tree-sitter-csharp.wasm", csharpWasm],
  ["assets/css/highlights.scm", cssHighlights],
  ["assets/css/tree-sitter-css.wasm", cssWasm],
  ["assets/go/highlights.scm", goHighlights],
  ["assets/go/tree-sitter-go.wasm", goWasm],
  ["assets/html/highlights.scm", htmlHighlights],
  ["assets/html/tree-sitter-html.wasm", htmlWasm],
  ["assets/java/highlights.scm", javaHighlights],
  ["assets/java/tree-sitter-java.wasm", javaWasm],
  ["assets/lua/highlights.scm", luaHighlights],
  ["assets/lua/tree-sitter-lua.wasm", luaWasm],
  ["assets/make/highlights.scm", makeHighlights],
  ["assets/make/tree-sitter-make.wasm", makeWasm],
  ["assets/javascript/highlights.scm", javascriptHighlights],
  ["assets/javascript/tree-sitter-javascript.wasm", javascriptWasm],
  ["assets/json/highlights.scm", jsonHighlights],
  ["assets/json/tree-sitter-json.wasm", jsonWasm],
  ["assets/markdown/highlights.scm", markdownHighlights],
  ["assets/markdown/injections.scm", markdownInjections],
  ["assets/markdown/tree-sitter-markdown.wasm", markdownWasm],
  ["assets/markdown_inline/highlights.scm", markdownInlineHighlights],
  ["assets/markdown_inline/tree-sitter-markdown_inline.wasm", markdownInlineWasm],
  ["assets/php/highlights.scm", phpHighlights],
  ["assets/php/tree-sitter-php.wasm", phpWasm],
  ["assets/python/highlights.scm", pythonHighlights],
  ["assets/python/tree-sitter-python.wasm", pythonWasm],
  ["assets/ruby/highlights.scm", rubyHighlights],
  ["assets/ruby/tree-sitter-ruby.wasm", rubyWasm],
  ["assets/rust/highlights.scm", rustHighlights],
  ["assets/rust/tree-sitter-rust.wasm", rustWasm],
  ["assets/toml/highlights.scm", tomlHighlights],
  ["assets/toml/tree-sitter-toml.wasm", tomlWasm],
  ["assets/typescript/highlights.scm", typescriptHighlights],
  ["assets/typescript/tree-sitter-typescript.wasm", typescriptWasm],
  ["assets/yaml/highlights.scm", yamlHighlights],
  ["assets/yaml/tree-sitter-yaml.wasm", yamlWasm],
  ["assets/zig/highlights.scm", zigHighlights],
  ["assets/zig/tree-sitter-zig.wasm", zigWasm],
  ["node_modules/web-tree-sitter/tree-sitter.js", webTreeSitterModule],
  ["node_modules/web-tree-sitter/tree-sitter.wasm", webTreeSitterWasm],
  ["web-tree-sitter/tree-sitter.wasm", webTreeSitterWasm],
] as const

export interface MaterializedTreeSitterRuntime {
  readonly root: string
  readonly workerPath: string
  readonly assetsPath: string
}

export function embeddedParserConfigurations(assets: string) {
  return [
    parserConfiguration(assets, "bash"),
    parserConfiguration(assets, "c"),
    parserConfiguration(assets, "cpp"),
    parserConfiguration(assets, "csharp"),
    parserConfiguration(assets, "css"),
    parserConfiguration(assets, "go"),
    parserConfiguration(assets, "html"),
    parserConfiguration(assets, "java"),
    parserConfiguration(assets, "lua"),
    parserConfiguration(assets, "make"),
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
          bash: "bash", sh: "bash", shell: "bash", zsh: "bash",
          c: "c", h: "c", cpp: "cpp", cc: "cpp", cxx: "cpp",
          csharp: "csharp", cs: "csharp", css: "css", go: "go", html: "html",
          java: "java", json: "json", lua: "lua", make: "make", makefile: "make",
          php: "php", python: "python", py: "python",
          ruby: "ruby", rb: "ruby", rust: "rust", rs: "rust", toml: "toml",
          yaml: "yaml", yml: "yaml",
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
    parserConfiguration(assets, "json"),
    parserConfiguration(assets, "php"),
    parserConfiguration(assets, "python"),
    parserConfiguration(assets, "ruby"),
    parserConfiguration(assets, "rust"),
    parserConfiguration(assets, "toml"),
    parserConfiguration(assets, "yaml"),
  ]
}

function parserConfiguration(assets: string, filetype: string) {
  return {
    filetype,
    queries: { highlights: [join(assets, filetype, "highlights.scm")] },
    wasm: join(assets, filetype, `tree-sitter-${filetype}.wasm`),
  }
}

/** Materialize Bun-embedded parser assets for OpenTUI's path-based worker API. */
export async function materializeTreeSitterRuntime(): Promise<MaterializedTreeSitterRuntime> {
  const cacheParent = join(
    process.env.ROTTWEILER_HOME?.trim() || join(homedir(), ".rottweiler"),
    "cache",
    "tree-sitter",
  )
  await mkdir(cacheParent, { recursive: true, mode: 0o700 })
  await requirePrivateDirectory(cacheParent)
  const root = join(cacheParent, TREE_SITTER_ASSET_DIGEST)
  if (await completedCache(root)) return materializedRuntime(root)

  const temporary = join(
    cacheParent,
    `.${TREE_SITTER_ASSET_DIGEST}.${process.pid}.${randomUUID()}.tmp`,
  )
  await mkdir(temporary, { mode: 0o700 })
  let total = 0
  try {
    let nextAsset = 0
    let failed = false
    const writeAssets = async (): Promise<void> => {
      while (!failed && nextAsset < embeddedAssets.length) {
        const [relative, embeddedPath] = embeddedAssets[nextAsset++]!
        let bytes = new Uint8Array(await Bun.file(embeddedPath).arrayBuffer())
        if (bytes[0] === 0x52 && bytes[1] === 0x57 && bytes[2] === 0x54 && bytes[3] === 0x5a) {
          if (bytes.byteLength <= COMPRESSED_ASSET_HEADER_BYTES) {
            throw new Error(`embedded Tree-sitter asset has a truncated header: ${relative}`)
          }
          const expectedBytes = new DataView(
            bytes.buffer,
            bytes.byteOffset + 4,
            4,
          ).getUint32(0, true)
          if (expectedBytes === 0 || expectedBytes > MAX_ASSET_BYTES) {
            throw new Error(`embedded Tree-sitter asset declares an invalid size: ${relative}`)
          }
          const compressed = bytes.subarray(COMPRESSED_ASSET_HEADER_BYTES)
          if (
            compressed[0] !== 0x28 ||
            compressed[1] !== 0xb5 ||
            compressed[2] !== 0x2f ||
            compressed[3] !== 0xfd
          ) {
            throw new Error(`embedded Tree-sitter asset is not a Zstandard frame: ${relative}`)
          }
          bytes = new Uint8Array(Bun.zstdDecompressSync(compressed))
          if (bytes.byteLength !== expectedBytes) {
            throw new Error(`embedded Tree-sitter asset size does not match its header: ${relative}`)
          }
        }
        if (relative === "parser.worker.js") {
          const source = new TextDecoder().decode(bytes)
          const external = 'from "web-tree-sitter"'
          const externalOccurrences = source.split(external).length - 1
          const bundledDependency =
            source.includes("node_modules/.bun/web-tree-sitter@0.25.10/") &&
            source.includes('resolveAssetPath("web-tree-sitter/tree-sitter.wasm"')
          if (externalOccurrences !== 1 && !bundledDependency) {
            throw new Error("embedded Tree-sitter worker has an unexpected dependency shape")
          }
          if (externalOccurrences === 1) {
            bytes = new TextEncoder().encode(
              source.replace(external, 'from "./node_modules/web-tree-sitter/tree-sitter.js"'),
            )
          }
        }
        if (bytes.byteLength === 0 || bytes.byteLength > MAX_ASSET_BYTES) {
          throw new Error(`embedded Tree-sitter asset has invalid size: ${relative}`)
        }
        total += bytes.byteLength
        if (total > MAX_RUNTIME_BYTES) {
          throw new Error("embedded Tree-sitter runtime exceeds its size limit")
        }
        const target = join(temporary, ...relative.split("/"))
        const directory = dirname(target)
        await mkdir(directory, { recursive: true, mode: 0o700 })
        await writeFile(target, bytes, { flag: "wx", mode: 0o600 })
      }
    }
    // A failed writer stops new admission. Already admitted writes retain
    // ownership until they settle, before either publication or cleanup.
    const writers = await Promise.allSettled(Array.from({ length: 4 }, async () => {
      try {
        await writeAssets()
      } catch (error) {
        failed = true
        throw error
      }
    }))
    for (const writer of writers) {
      if (writer.status === "rejected") throw writer.reason
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
    await writeFile(join(temporary, "node_modules/web-tree-sitter/package.json"), packageManifest, {
      flag: "wx",
      mode: 0o600,
    })
    await writeFile(join(temporary, ".complete"), `${TREE_SITTER_ASSET_DIGEST}\n`, {
      flag: "wx",
      mode: 0o600,
    })
    try {
      await rename(temporary, root)
    } catch (error) {
      if (!(error instanceof Error) || !("code" in error) ||
        (error.code !== "EEXIST" && error.code !== "ENOTEMPTY")) throw error
      await rm(temporary, { recursive: true, force: true })
      if (!(await completedCache(root))) {
        throw new Error("Tree-sitter cache publication raced with an invalid entry")
      }
    }
  } catch (error) {
    await rm(temporary, { recursive: true, force: true })
    throw error
  }
  return materializedRuntime(root)
}

function materializedRuntime(root: string): MaterializedTreeSitterRuntime {
  return {
    root,
    workerPath: join(root, "parser.worker.js"),
    assetsPath: join(root, "assets"),
  }
}

async function requirePrivateDirectory(path: string): Promise<void> {
  const metadata = await lstat(path)
  const currentUid = typeof process.getuid === "function" ? process.getuid() : null
  if (
    !metadata.isDirectory() ||
    metadata.isSymbolicLink() ||
    (metadata.mode & 0o777) !== 0o700 ||
    (currentUid !== null && metadata.uid !== currentUid)
  ) {
    throw new Error(`Tree-sitter cache directory is not private: ${path}`)
  }
}

async function completedCache(root: string): Promise<boolean> {
  try {
    await requirePrivateDirectory(root)
    const marker = join(root, ".complete")
    const metadata = await lstat(marker)
    const currentUid = typeof process.getuid === "function" ? process.getuid() : null
    if (
      !metadata.isFile() ||
      metadata.isSymbolicLink() ||
      (metadata.mode & 0o777) !== 0o600 ||
      (currentUid !== null && metadata.uid !== currentUid)
    ) return false
    return (await readFile(marker, "utf8")) === `${TREE_SITTER_ASSET_DIGEST}\n`
  } catch {
    return false
  }
}
