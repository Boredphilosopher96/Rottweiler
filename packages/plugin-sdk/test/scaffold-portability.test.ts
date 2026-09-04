import { describe, expect, test } from "bun:test"
import { cp, mkdir, mkdtemp, readFile, readdir, rm, symlink, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { pathToFileURL } from "node:url"

const packageRoot = join(import.meta.dir, "..")
const canonicalMapping = await readFile(join(packageRoot, "fixtures/scaffold/files.txt"), "utf8")

async function withMapping(
  mapping: string,
  check: (scaffold: typeof import("../src/scaffold"), root: string) => Promise<void>,
): Promise<void> {
  const root = await mkdtemp(join(tmpdir(), "rottweiler-scaffold-portability-"))
  try {
    await mkdir(join(root, "src"))
    await cp(join(packageRoot, "src/scaffold.ts"), join(root, "src/scaffold.ts"))
    await cp(join(packageRoot, "fixtures"), join(root, "fixtures"), { recursive: true })
    await writeFile(join(root, "fixtures/scaffold/files.txt"), mapping)
    const scaffold: typeof import("../src/scaffold") = await import(pathToFileURL(join(root, "src/scaffold.ts")).href)
    await check(scaffold, root)
  } finally {
    await rm(root, { recursive: true, force: true })
  }
}

describe("scaffold mapping portability", () => {
  for (const ending of ["\n", "\r\n"]) {
    test(`generates and protects the same paths with ${JSON.stringify(ending)} lines`, async () => {
      await withMapping(canonicalMapping.replace(/\r?\n/g, ending), async (scaffold, root) => {
        const destination = join(root, "plugin")
        const expected = ["package.json", "tsconfig.json", "manifest.json", "src/index.ts", "test/plugin.test.ts", ".gitignore"]
        expect(scaffold.renderTypeScriptScaffold().map((file) => file.path)).toEqual(expected)
        expect(await scaffold.scaffoldTypeScriptPlugin(destination, { name: "portable" })).toEqual(expected)
        expect(await readFile(join(destination, "manifest.json"), "utf8")).toContain('"name": "portable"')
        await expect(scaffold.scaffoldTypeScriptPlugin(destination)).rejects.toMatchObject({ code: "EEXIST" })
        await scaffold.scaffoldTypeScriptPlugin(destination, { force: true })

        await rm(destination, { recursive: true })
        await mkdir(destination)
        const sentinel = join(root, "sentinel.json")
        await writeFile(sentinel, "unchanged")
        await symlink(sentinel, join(destination, "package.json"))
        await expect(scaffold.scaffoldTypeScriptPlugin(destination, { force: true })).rejects.toThrow("symlink")
        expect(await readFile(sentinel, "utf8")).toBe("unchanged")
        expect(await readdir(destination)).toEqual(["package.json"])
      })
    })
  }

  for (const destination of ["", ".", "..", "../escape", "/absolute", "src/../escape", "src//index.ts", "src/", "C:/escape", "src\\index.ts", "manifest\r.json", "bad\0name", "bad\u007fname", "trailing.", "trailing ", "CON.json", "bad?name"]) {
    test(`rejects invalid destination ${JSON.stringify(destination)} before writing`, async () => {
      await withMapping(`package.json\t${destination}\n`, async (scaffold, root) => {
        const output = join(root, "plugin")
        await expect(scaffold.scaffoldTypeScriptPlugin(output)).rejects.toThrow("invalid canonical scaffold")
        await expect(readdir(output)).rejects.toMatchObject({ code: "ENOENT" })
      })
    })
  }

  test("rejects a source path outside the canonical template", async () => {
    await withMapping("../wire/protocol-2.json\tpackage.json\n", async (scaffold) => {
      expect(() => scaffold.renderTypeScriptScaffold()).toThrow("invalid canonical scaffold")
    })
  })
})
