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
  return [
    {
      path: "package.json",
      contents: `${JSON.stringify(
        {
          name,
          version: "0.1.0",
          private: true,
          type: "module",
          scripts: {
            build: "mkdir -p dist && bun build --compile src/index.ts --outfile dist/plugin",
            start: "bun run src/index.ts",
            test: "bun test",
            typecheck: "tsc --noEmit",
          },
          dependencies: { "@rottweiler/plugin": "^0.1.0" },
          devDependencies: { "@types/bun": "^1.3.14", typescript: "^7.0.2" },
        },
        null,
        2,
      )}\n`,
    },
    {
      path: "tsconfig.json",
      contents: `${JSON.stringify(
        {
          compilerOptions: {
            exactOptionalPropertyTypes: true,
            module: "Preserve",
            moduleResolution: "Bundler",
            noEmit: true,
            strict: true,
            target: "ES2022",
            types: ["bun"],
          },
          include: ["src/**/*.ts", "test/**/*.ts"],
        },
        null,
        2,
      )}\n`,
    },
    {
      path: "manifest.json",
      contents: `${JSON.stringify(
        {
          name,
          version: "0.1.0",
          protocol: 1,
          capabilities: {
            tools: [{
              name: "hello",
              description: "Return a greeting",
              schema: { type: "object", properties: { name: { type: "string" } } },
              caps: ["reads-fs"],
            }],
            hooks: [{ name: "pre_tool", failure_policy: "fail-closed" }],
          },
        },
        null,
        2,
      )}\n`,
    },
    {
      path: "src/index.ts",
      contents: `import { definePlugin, runPlugin } from "@rottweiler/plugin"\n\nexport const plugin = definePlugin({\n  manifest: {\n    name: ${JSON.stringify(name)},\n    version: "0.1.0",\n    protocol: 1,\n    capabilities: {\n      tools: [{\n        name: "hello",\n        description: "Return a greeting",\n        schema: { type: "object", properties: { name: { type: "string" } } },\n        caps: ["reads-fs"],\n      }],\n      hooks: [{ name: "pre_tool", failure_policy: "fail-closed" }],\n    },\n  },\n  handlers: {\n    tools: {\n      hello: ({ input }) => ({\n        content: \`Hello, \${String(input.name ?? "world")}!\`,\n        data: { text: \`Hello, \${String(input.name ?? "world")}!\` },\n      }),\n    },\n    hooks: {\n      pre_tool: ({ payload }) =>\n        payload.name === "bash"\n          ? { decision: "deny", message: "This plugin blocks shell execution" }\n          : { decision: "allow" },\n    },\n  },\n})\n\nif (import.meta.main) await runPlugin(plugin)\n`,
    },
    {
      path: "test/plugin.test.ts",
      contents: `import { expect, test } from "bun:test"\nimport { plugin } from "../src/index"\n\ntest("declares a fail-closed pre_tool hook and custom tool", () => {\n  expect(plugin.manifest.capabilities.tools?.[0]?.name).toBe("hello")\n  expect(plugin.manifest.capabilities.hooks?.[0]).toEqual({\n    name: "pre_tool",\n    failure_policy: "fail-closed",\n  })\n})\n`,
    },
    {
      path: ".gitignore",
      contents: "node_modules/\ndist/\n",
    },
  ]
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
