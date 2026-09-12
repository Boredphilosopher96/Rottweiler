import { expect, test } from "bun:test"
import { cp, mkdtemp, readFile, rm, symlink } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { renderTypeScriptScaffold } from "../src/scaffold"

test("scaffolds use the SDK's exact checked declaration graph and runtime", async () => {
  const sdk = JSON.parse(await readFile(join(import.meta.dir, "../package.json"), "utf8"))
  const files = renderTypeScriptScaffold()
  const generated = JSON.parse(files.find((file) => file.path === "package.json")!.contents)
  expect(generated.dependencies).toEqual({ [sdk.name]: sdk.version })
  expect(generated.devDependencies).toEqual(sdk.devDependencies)
  for (const version of Object.values(generated.devDependencies)) {
    expect(version).toMatch(/^\d+\.\d+\.\d+$/)
  }
  expect(generated.packageManager).toBe(sdk.packageManager)
  expect(generated.engines).toEqual(sdk.engines)
  const sourceConfig = JSON.parse(await readFile(join(import.meta.dir, "../tsconfig.json"), "utf8"))
  const generatedConfig = JSON.parse(files.find((file) => file.path === "tsconfig.json")!.contents)
  expect(sourceConfig.compilerOptions.skipLibCheck).not.toBe(true)
  expect(generatedConfig.compilerOptions.skipLibCheck).not.toBe(true)
})

test("packing a clean SDK builds every exported runtime and declaration", async () => {
  const root = await mkdtemp(join(tmpdir(), "rottweiler-clean-sdk-pack-"))
  const source = join(import.meta.dir, "..")
  try {
    for (const input of ["build.ts", "package.json", "tsconfig.json", "tsconfig.build.json", "src", "fixtures"]) {
      await cp(join(source, input), join(root, input), { recursive: true })
    }
    await symlink(join(source, "node_modules"), join(root, "node_modules"), "dir")
    const packed = Bun.spawnSync(["npm", "pack", "--json", "--pack-destination", root], {
      cwd: root, stdout: "pipe", stderr: "pipe", timeout: 30_000,
    })
    expect(packed.exitCode, packed.stderr.toString()).toBe(0)
    const result: unknown = JSON.parse(packed.stdout.toString())
    if (!Array.isArray(result) || result.length !== 1) throw new Error("expected one packed SDK")
    const archive = result[0]
    if (typeof archive !== "object" || archive === null || !Array.isArray(archive.files)) {
      throw new Error("npm pack omitted its file inventory")
    }
    const files = archive.files.map((entry: { path: string }) => entry.path)
    for (const path of ["dist/index.js", "dist/index.d.ts", "dist/scaffold.js", "dist/scaffold.d.ts", "dist/bin/scaffold.js"]) {
      expect(files).toContain(path)
    }
  } finally {
    await rm(root, { recursive: true, force: true })
  }
}, 40_000)
