import { readFileSync } from "node:fs"
import { lstat, mkdir, realpath, rename, rm, writeFile } from "node:fs/promises"
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path"

export interface ScaffoldOptions {
  readonly name?: string
  readonly force?: boolean
}

export interface ScaffoldFile {
  readonly path: string
  readonly contents: string
}

function packageName(name: string): string {
  const normalized = name.trim().toLowerCase().replace(/[^a-z0-9-]+/g, "-").replace(/^-+|-+$/g, "")
  if (normalized === "") throw new Error("plugin name must contain a letter or number")
  return normalized
}

/** Deterministic template consumed by `rw plugin scaffold --lang ts`. */
export function renderTypeScriptScaffold(options: ScaffoldOptions = {}): readonly ScaffoldFile[] {
  const name = packageName(options.name ?? "rottweiler-plugin")
  const root = resolve(import.meta.dir, "../fixtures/scaffold")
  return readFileSync(join(root, "files.txt"), "utf8")
    .trimEnd()
    .split("\n")
    .map((line) => {
      const [source, path, unexpected] = line.split("\t")
      if (source === undefined || path === undefined || unexpected !== undefined) {
        throw new Error("invalid canonical scaffold file mapping")
      }
      return {
        path,
        contents: readFileSync(join(root, source), "utf8")
          .replaceAll("__ROTTWEILER_PLUGIN_NAME__", name),
      }
    })
}

export async function scaffoldTypeScriptPlugin(
  destination: string,
  options: ScaffoldOptions = {},
): Promise<readonly string[]> {
  const files = renderTypeScriptScaffold(options)
  let root = resolve(destination)

  const statOrUndefined = async (path: string) => {
    try {
      return await lstat(path)
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") return undefined
      throw error
    }
  }

  let rootStat = await statOrUndefined(root)
  if (rootStat?.isSymbolicLink() === true || (rootStat !== undefined && !rootStat.isDirectory())) {
    throw new Error("scaffold destination must be a real directory")
  }

  // Resolve intermediate symlinks before creating an absent destination. This
  // avoids writing through a surprising ancestor while still supporting normal
  // macOS paths such as /var (a system symlink to /private/var).
  if (rootStat === undefined) {
    let existing = dirname(root)
    while ((await statOrUndefined(existing)) === undefined) existing = dirname(existing)
    const suffix = relative(existing, root)
    root = join(await realpath(existing), suffix)
    rootStat = await statOrUndefined(root)
    if (rootStat?.isSymbolicLink() === true || (rootStat !== undefined && !rootStat.isDirectory())) {
      throw new Error("scaffold destination must resolve to a real directory")
    }
  } else {
    root = await realpath(root)
  }

  // Preflight the complete fixed template before touching the destination. A
  // failed default scaffold therefore cannot leave a misleading half-project.
  for (const file of files) {
    const target = join(root, file.path)
    const fromRoot = relative(root, target)
    if (fromRoot === ".." || fromRoot.startsWith(`..${sep}`) || isAbsolute(fromRoot)) {
      throw new Error("scaffold template escaped its destination")
    }
    const parent = await statOrUndefined(dirname(target))
    if (parent?.isSymbolicLink() === true || (parent !== undefined && !parent.isDirectory())) {
      throw new Error(`scaffold parent is not a real directory: ${file.path}`)
    }
    const targetStat = await statOrUndefined(target)
    if (targetStat?.isSymbolicLink() === true) throw new Error(`refusing to replace symlink: ${file.path}`)
    if (targetStat !== undefined && !targetStat.isFile()) {
      throw new Error(`scaffold target is not a regular file: ${file.path}`)
    }
    if (targetStat !== undefined && options.force !== true) {
      const error = new Error(`scaffold target already exists: ${file.path}`) as NodeJS.ErrnoException
      error.code = "EEXIST"
      throw error
    }
  }

  for (const file of files) {
    const target = join(root, file.path)
    await mkdir(dirname(target), { recursive: true })
    const temporary = `${target}.rottweiler-${crypto.randomUUID()}.tmp`
    try {
      await writeFile(temporary, file.contents, { encoding: "utf8", flag: "wx", mode: 0o644 })
      await rename(temporary, target)
    } finally {
      await rm(temporary, { force: true })
    }
  }
  return files.map((file) => file.path)
}
