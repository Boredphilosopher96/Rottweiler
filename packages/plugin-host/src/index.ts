import { lstat, realpath } from "node:fs/promises"
import { builtinModules } from "node:module"
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path"
import { pathToFileURL } from "node:url"

import { runPlugin, type PluginDefinition } from "@rottweiler/plugin"

import { SOURCE_HOST_ABI, SOURCE_BUNDLE_FORMAT } from "./protocol"

interface GraphInput {
  readonly path: string
  readonly bytes: number
}

interface GraphReport {
  readonly abi: typeof SOURCE_HOST_ABI
  readonly format: typeof SOURCE_BUNDLE_FORMAT
  readonly inputs: readonly GraphInput[]
}

const NODE_BUILTINS = new Set(builtinModules.flatMap((name) => [name, `node:${name}`]))

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}

function isPluginDefinition(value: unknown): value is PluginDefinition {
  return isRecord(value) && isRecord(value.manifest) && isRecord(value.handlers)
}

async function canonicalRootAndEntry(rootArgument: string, entryArgument: string): Promise<{
  readonly root: string
  readonly entry: string
}> {
  if (!isAbsolute(rootArgument) || !isAbsolute(entryArgument)) {
    throw new Error("source host paths must be absolute")
  }
  const root = await realpath(rootArgument)
  const entry = await realpath(entryArgument)
  const rootStat = await lstat(root)
  const entryStat = await lstat(entry)
  if (!rootStat.isDirectory() || !entryStat.isFile() || entryStat.isSymbolicLink()) {
    throw new Error("source host root or entry is invalid")
  }
  const fromRoot = relative(root, entry)
  if (fromRoot === "" || fromRoot === ".." || fromRoot.startsWith(`..${sep}`) || isAbsolute(fromRoot)) {
    throw new Error("source host entry escapes its package root")
  }
  return { root, entry }
}

async function rejectSymlinkComponents(root: string, logicalPath: string): Promise<void> {
  let current = root
  for (const component of logicalPath.split(/[\\/]/u)) {
    if (component === "" || component === "." || component === "..") {
      throw new Error("plugin source graph contains an invalid path component")
    }
    current = join(current, component)
    if ((await lstat(current)).isSymbolicLink()) {
      throw new Error("plugin source graph contains a symlink")
    }
  }
}

async function rejectRelativeImportSymlink(
  root: string,
  importer: string,
  original: string | undefined,
): Promise<void> {
  if (original === undefined || (!original.startsWith("./") && !original.startsWith("../"))) return
  const requested = resolve(dirname(importer), original)
  const logical = relative(root, requested)
  if (logical === "" || logical === ".." || logical.startsWith(`..${sep}`) || isAbsolute(logical)) {
    throw new Error("plugin source graph escapes its package root")
  }
  try {
    await rejectSymlinkComponents(root, logical)
  } catch (error) {
    if (isRecord(error) && error.code === "ENOENT") return
    throw error
  }
}

async function buildGraph(root: string, entry: string, outdir?: string): Promise<GraphReport> {
  const result = await Bun.build({
    entrypoints: [entry],
    root,
    ...(outdir === undefined
      ? { write: false }
      : { outdir, naming: "plugin.mjs", write: true }),
    target: "bun",
    format: "esm",
    splitting: false,
    minify: true,
    sourcemap: "none",
    metafile: true,
  })
  if (!result.success || result.metafile === undefined) {
    throw new AggregateError(result.logs, "plugin source build failed")
  }
  const inputPaths = new Set<string>(Object.keys(result.metafile.inputs))
  inputPaths.add(join(root, "package.json"))
  for (const inputPath of [...inputPaths]) {
    const parts = inputPath.split("/")
    const nodeModules = parts.lastIndexOf("node_modules")
    if (nodeModules < 0 || parts[nodeModules + 1] === undefined) continue
    const packageParts = parts[nodeModules + 1]?.startsWith("@") ? 2 : 1
    inputPaths.add(resolve(process.cwd(), join(...parts.slice(0, nodeModules + 1 + packageParts), "package.json")))
  }
  const inputs: GraphInput[] = []
  for (const inputPath of inputPaths) {
    const absolute = isAbsolute(inputPath) ? inputPath : resolve(process.cwd(), inputPath)
    const input = result.metafile.inputs[inputPath]
    for (const imported of input?.imports ?? []) {
      if (imported.kind === "dynamic-import") {
        throw new Error("plugin source contains a dynamic import")
      }
      if (imported.external && !NODE_BUILTINS.has(imported.path)) {
        throw new Error("plugin source contains an unresolved external import")
      }
      await rejectRelativeImportSymlink(root, absolute, imported.original)
    }
    const lexical = relative(root, absolute)
    if (lexical === "" || lexical === ".." || lexical.startsWith(`..${sep}`) || isAbsolute(lexical)) {
      throw new Error("plugin source graph escapes its package root")
    }
    await rejectSymlinkComponents(root, lexical)
    const canonical = await realpath(absolute)
    const fromRoot = relative(root, canonical)
    if (fromRoot === "" || fromRoot === ".." || fromRoot.startsWith(`..${sep}`) || isAbsolute(fromRoot)) {
      throw new Error("plugin source graph escapes its package root")
    }
    if (canonical.endsWith(".node")) throw new Error("plugin source graph contains a native addon")
    const stat = await lstat(canonical)
    if (!stat.isFile() || stat.isSymbolicLink()) throw new Error("plugin source input is not a regular file")
    inputs.push({ path: fromRoot.split(sep).join("/"), bytes: stat.size })
  }
  inputs.sort((left, right) => left.path.localeCompare(right.path))
  if (inputs.length === 0 || new Set(inputs.map((input) => input.path.toLowerCase())).size !== inputs.length) {
    throw new Error("plugin source graph is empty or has a case-fold collision")
  }
  return { abi: SOURCE_HOST_ABI, format: SOURCE_BUNDLE_FORMAT, inputs }
}

async function graph(rootArgument: string, entryArgument: string): Promise<void> {
  const { root, entry } = await canonicalRootAndEntry(rootArgument, entryArgument)
  process.stdout.write(`${JSON.stringify(await buildGraph(root, entry))}\n`)
}

async function bundle(rootArgument: string, entryArgument: string, outputArgument: string): Promise<void> {
  const { root, entry } = await canonicalRootAndEntry(rootArgument, entryArgument)
  if (!isAbsolute(outputArgument)) throw new Error("source host output must be absolute")
  process.stdout.write(`${JSON.stringify(await buildGraph(root, entry, outputArgument))}\n`)
}

async function run(bundleArgument: string): Promise<void> {
  if (!isAbsolute(bundleArgument)) throw new Error("source host bundle path must be absolute")
  const bundlePath = await realpath(bundleArgument)
  const stat = await lstat(bundlePath)
  if (!stat.isFile() || stat.isSymbolicLink()) throw new Error("source host bundle is invalid")
  const loaded: unknown = await import(pathToFileURL(bundlePath).href)
  if (!isRecord(loaded) || !isPluginDefinition(loaded.plugin)) {
    throw new Error("source bundle must export one plugin definition named plugin")
  }
  await runPlugin(loaded.plugin)
}

export async function main(argv: readonly string[]): Promise<void> {
  const [command, ...args] = argv
  if (command === "version" && args.length === 0) {
    process.stdout.write(`${JSON.stringify({ abi: SOURCE_HOST_ABI, format: SOURCE_BUNDLE_FORMAT })}\n`)
    return
  }
  if (command === "graph" && args.length === 2 && args[0] !== undefined && args[1] !== undefined) {
    await graph(args[0], args[1])
    return
  }
  if (
    command === "bundle" &&
    args.length === 3 &&
    args[0] !== undefined &&
    args[1] !== undefined &&
    args[2] !== undefined
  ) {
    await bundle(args[0], args[1], args[2])
    return
  }
  if (command === "run" && args.length === 1 && args[0] !== undefined) {
    await run(args[0])
    return
  }
  throw new Error("usage: rottweiler-js-host source-plugin version|graph ROOT ENTRY|bundle ROOT ENTRY OUTDIR|run BUNDLE")
}
